//! Trusted-host memory store. Grants and drafts are not model authority. Agent
//! tool registration/context revalidation are separate integration obligations.
use crate::{Journal, JournalError};
use orange_protocol::{request_digest, valid_id};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
mod directory;
mod drafts;
pub use directory::{MemoryMemberItem, MemoryMemberPage, MemoryScopeItem, MemoryScopePage};
#[cfg(test)]
mod tests;
pub mod tools;
pub use drafts::{
    MemoryDraftOutcome, MemoryMutation, MemoryReview, MemoryWriteRequest, PreparedMemoryDraft,
};
const MAX_PAYLOAD: usize = 32 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("invalid memory request")]
    Invalid,
    #[error("memory permission is unavailable or revoked")]
    Denied,
    #[error("memory version or request conflicts")]
    Conflict,
    #[error("memory source authorization required")]
    SourceRequired,
    #[error("memory quota exceeded")]
    Limit,
    #[error("exact current user approval required")]
    ApprovalRequired,
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Journal(#[from] JournalError),
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAccess {
    Read,
    Write,
    Revoked,
}
impl MemoryAccess {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Revoked => "revoked",
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOrigin {
    UserDeclaration,
    ModelInference,
}
impl MemoryOrigin {
    fn name(self) -> &'static str {
        match self {
            Self::UserDeclaration => "user_declaration",
            Self::ModelInference => "model_inference",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryPayload {
    pub title: String,
    pub content: String,
    pub enabled: bool,
}
impl MemoryPayload {
    fn validate(&self) -> Result<(), MemoryError> {
        if self.title.is_empty()
            || self.title.len() > 128
            || self.title.chars().any(char::is_control)
            || self.content.len() > MAX_PAYLOAD
            || self.content.contains('\0')
            || encoded(self)?.len() > MAX_PAYLOAD
        {
            Err(MemoryError::Invalid)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryScope {
    pub scope_id: String,
    pub owner: String,
    pub shared: bool,
    pub revision: String,
    pub enabled: bool,
}
pub(crate) struct ScopeSignal {
    closed: AtomicBool,
    notice: tokio::sync::watch::Sender<bool>,
    gate: Arc<tokio::sync::RwLock<()>>,
}
impl ScopeSignal {
    pub(crate) fn revoke(&self) {
        self.closed.store(true, Ordering::Release);
        self.notice.send_replace(true);
    }
}
#[derive(Clone)]
pub struct MemoryScopeRevoker {
    signals: Vec<Arc<ScopeSignal>>,
}
impl MemoryScopeRevoker {
    pub fn request_revoke(&self) {
        for signal in &self.signals {
            signal.revoke();
        }
    }
    pub async fn wait_drained(&self) {
        for signal in &self.signals {
            drop(signal.gate.clone().write_owned().await);
        }
    }
}
pub(crate) struct MemoryLease {
    grant: MemoryGrant,
    _local: tokio::sync::OwnedRwLockReadGuard<()>,
    _scope: tokio::sync::OwnedRwLockReadGuard<()>,
}
impl MemoryLease {
    pub(crate) fn grant(&self) -> MemoryGrant {
        self.grant.clone()
    }
    pub(crate) fn is_live(&self) -> bool {
        !self.grant.is_revoked()
    }
    pub(crate) fn check_db(&self, db: &Connection, session: &str) -> Result<(), MemoryError> {
        check(db, &self.grant, self.grant.write, session)
    }
}
#[derive(Clone)]
pub struct MemoryGrant {
    actor: String,
    session: String,
    scope: String,
    revision: i64,
    epoch: i64,
    write: bool,
    policy: String,
    closed: Arc<AtomicBool>,
    revoked: tokio::sync::watch::Sender<bool>,
    gate: Arc<tokio::sync::RwLock<()>>,
    scope_signal: Arc<ScopeSignal>,
}
impl MemoryGrant {
    /// Signal only. Drop cached read receipts, then await actual lease drain and
    /// the serialized Journal mutation before acknowledging closure. Scope-wide
    /// revocation uses MemoryScopeRevoker, not a single session's local signal.
    pub fn request_revoke(&self) {
        self.closed.store(true, Ordering::Release);
        self.revoked.send_replace(true);
    }
    pub fn is_revoked(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.scope_signal.closed.load(Ordering::Acquire)
    }
    pub async fn wait_drained(&self) {
        drop(self.gate.clone().write_owned().await);
    }
    pub(crate) fn lease(&self) -> Result<MemoryLease, MemoryError> {
        if self.is_revoked() {
            return Err(MemoryError::Denied);
        }
        let local = self
            .gate
            .clone()
            .try_read_owned()
            .map_err(|_| MemoryError::Denied)?;
        let scope = self
            .scope_signal
            .gate
            .clone()
            .try_read_owned()
            .map_err(|_| MemoryError::Denied)?;
        if self.is_revoked() {
            return Err(MemoryError::Denied);
        }
        Ok(MemoryLease {
            grant: self.clone(),
            _local: local,
            _scope: scope,
        })
    }
    pub(crate) async fn revoked(&self) {
        let mut local = self.revoked.subscribe();
        let mut scope = self.scope_signal.notice.subscribe();
        while !self.is_revoked() {
            tokio::select! {_=local.changed()=>{},_=scope.changed()=>{}}
        }
    }
    pub(crate) fn actor(&self) -> &str {
        &self.actor
    }
    pub(crate) fn policy(&self) -> &str {
        &self.policy
    }
    pub fn scope_id(&self) -> &str {
        &self.scope
    }
}
fn encoded<T: Serialize>(v: &T) -> Result<String, MemoryError> {
    serde_json::to_string(v).map_err(|_| MemoryError::Invalid)
}
fn digest<T: Serialize>(kind: &str, v: &T) -> Result<String, MemoryError> {
    request_digest(kind, v).map_err(|_| MemoryError::Invalid)
}
fn version(v: &str) -> Result<i64, MemoryError> {
    let n = v.parse::<i64>().map_err(|_| MemoryError::Invalid)?;
    if n < 1 || n.to_string() != v {
        return Err(MemoryError::Invalid);
    }
    Ok(n)
}
fn now() -> Result<i64, MemoryError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| MemoryError::Invalid)?
        .as_secs()
        .try_into()
        .map_err(|_| MemoryError::Invalid)
}
fn check(
    db: &Connection,
    grant: &MemoryGrant,
    write: bool,
    session: &str,
) -> Result<(), MemoryError> {
    if grant.session != session || grant.is_revoked() || write && !grant.write {
        return Err(MemoryError::Denied);
    }
    let permitted:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM memory_scopes s JOIN memory_members m USING(scope_id) JOIN runtime_host h ON h.singleton=1 WHERE s.scope_id=? AND s.enabled=1 AND s.revision=? AND m.principal=? AND m.access IN('read','write') AND (?=0 OR m.access='write') AND h.epoch=?)",params![grant.scope,grant.revision,grant.actor,write,grant.epoch],|r|r.get(0))?;
    if !permitted {
        return Err(MemoryError::Denied);
    }
    Ok(())
}
impl Journal {
    pub fn create_memory_scope(
        &mut self,
        owner: &str,
        id: &str,
        shared: bool,
    ) -> Result<MemoryScope, MemoryError> {
        if !valid_id(owner) || !valid_id(id) {
            return Err(MemoryError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(String, bool)> = tx
            .query_row(
                "SELECT owner,shared FROM memory_scopes WHERE scope_id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((actor, kind)) = old {
            if actor != owner || kind != shared {
                return Err(MemoryError::Conflict);
            }
        } else {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM memory_scopes WHERE owner=?",
                [owner],
                |r| r.get(0),
            )?;
            if count >= 128 {
                return Err(MemoryError::Limit);
            }
            tx.execute(
                "INSERT INTO memory_scopes VALUES(?,?,?,1,1)",
                params![id, owner, shared],
            )?;
            tx.execute(
                "INSERT INTO memory_members VALUES(?,?,'write')",
                params![id, owner],
            )?;
            tx.execute("INSERT INTO memory_audit(scope_id,actor,action,revision) VALUES(?,?,'scope_created',1)",params![id,owner])?;
        }
        tx.commit()?;
        self.memory_scope(owner, id)
    }
    pub fn memory_scope(&self, actor: &str, id: &str) -> Result<MemoryScope, MemoryError> {
        if !valid_id(actor) || !valid_id(id) {
            return Err(MemoryError::Invalid);
        }
        self.connection.query_row("SELECT s.scope_id,s.owner,s.shared,s.revision,s.enabled FROM memory_scopes s JOIN memory_members m USING(scope_id) WHERE s.scope_id=? AND m.principal=? AND m.access!='revoked'",params![id,actor],|r|Ok(MemoryScope{scope_id:r.get(0)?,owner:r.get(1)?,shared:r.get(2)?,revision:r.get::<_,i64>(3)?.to_string(),enabled:r.get(4)?})).optional()?.ok_or(MemoryError::Denied)
    }
    /// Authenticated scope-owner administration only; no team-role inheritance.
    pub fn set_memory_member(
        &mut self,
        owner: &str,
        scope: &str,
        principal: &str,
        access: MemoryAccess,
        expected: &str,
    ) -> Result<MemoryScope, MemoryError> {
        if ![owner, scope, principal].iter().all(|v| valid_id(v)) || owner == principal {
            return Err(MemoryError::Invalid);
        }
        let revision = version(expected)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let allowed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_scopes WHERE scope_id=? AND owner=? AND shared=1 AND revision=?)",params![scope,owner,revision],|r|r.get(0))?;
        if !allowed {
            return Err(MemoryError::Denied);
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM memory_members WHERE scope_id=? AND principal!=?",
            params![scope, principal],
            |r| r.get(0),
        )?;
        if count >= 64 {
            return Err(MemoryError::Limit);
        }
        tx.execute("INSERT INTO memory_members VALUES(?,?,?) ON CONFLICT(scope_id,principal) DO UPDATE SET access=excluded.access",params![scope,principal,access.name()])?;
        let next = revision.checked_add(1).ok_or(MemoryError::Limit)?;
        tx.execute(
            "UPDATE memory_scopes SET revision=? WHERE scope_id=?",
            params![next, scope],
        )?;
        tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE scope_id=? AND state='pending'",[scope])?;
        tx.execute("INSERT INTO memory_audit(scope_id,actor,action,revision,digest) VALUES(?,?,'membership_changed',?,?)",params![scope,owner,next,digest("memory-membership.v1",&(principal,access))?])?;
        tx.commit()?;
        self.memory_scope_signal(scope, revision).revoke();
        self.memory_scope(owner, scope)
    }
    pub fn set_memory_scope_enabled(
        &mut self,
        owner: &str,
        scope: &str,
        expected: &str,
        enabled: bool,
    ) -> Result<MemoryScope, MemoryError> {
        if !valid_id(owner) || !valid_id(scope) {
            return Err(MemoryError::Invalid);
        }
        let revision = version(expected)?;
        let next = revision.checked_add(1).ok_or(MemoryError::Limit)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE memory_scopes SET revision=?,enabled=? WHERE scope_id=? AND owner=? AND revision=?",params![next,enabled,scope,owner,revision])?!=1{return Err(MemoryError::Conflict);}
        tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE scope_id=? AND state='pending'",[scope])?;
        tx.execute(
            "INSERT INTO memory_audit(scope_id,actor,action,revision) VALUES(?,?,?,?)",
            params![
                scope,
                owner,
                if enabled {
                    "scope_enabled"
                } else {
                    "scope_disabled"
                },
                next
            ],
        )?;
        tx.commit()?;
        self.memory_scope_signal(scope, revision).revoke();
        self.memory_scope(owner, scope)
    }
    pub fn authorize_memory(
        &self,
        actor: &str,
        scope: &str,
        policy: &str,
        write: bool,
    ) -> Result<MemoryGrant, MemoryError> {
        if ![actor, scope, policy].iter().all(|v| valid_id(v)) {
            return Err(MemoryError::Invalid);
        }
        let snapshot = self.memory_scope(actor, scope)?;
        let grant = MemoryGrant {
            actor: actor.into(),
            session: self.memory_session.clone(),
            scope: scope.into(),
            revision: version(&snapshot.revision)?,
            epoch: self.connection.query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )?,
            write,
            policy: policy.into(),
            closed: Arc::new(AtomicBool::new(false)),
            revoked: tokio::sync::watch::channel(false).0,
            gate: Arc::new(tokio::sync::RwLock::new(())),
            scope_signal: self.memory_scope_signal(scope, version(&snapshot.revision)?),
        };
        check(&self.connection, &grant, write, &self.memory_session)?;
        Ok(grant)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryValue {
    pub scope_id: String,
    pub entry_id: String,
    pub owner: String,
    pub version: String,
    pub title: String,
    pub content: String,
    pub enabled: bool,
    pub origin: MemoryOrigin,
    pub source_actor: String,
    pub source_run: String,
    pub total_bytes: String,
    pub start_byte: String,
    pub next_byte: Option<String>,
}
pub struct MemoryReadReceipt {
    grant: MemoryGrant,
    value: MemoryValue,
    include_disabled: bool,
    _lease: MemoryLease,
}
pub(crate) struct MemorySourceLease {
    receipt: MemoryReadReceipt,
}
impl MemorySourceLease {
    pub(crate) fn check_connection(
        &self,
        db: &Connection,
        session: &str,
        changed: Option<(&str, &str, i64)>,
    ) -> Result<(), MemoryError> {
        self.receipt.check_connection(db, session, changed)
    }
    pub(crate) fn is_authorized(&self) -> bool {
        self.receipt.is_authorized()
    }
    pub(crate) fn grant(&self) -> MemoryGrant {
        self.receipt.grant()
    }
}
impl MemoryReadReceipt {
    pub(crate) fn into_source_lease(mut self) -> MemorySourceLease {
        self.value.title = String::new();
        self.value.content = String::new();
        MemorySourceLease { receipt: self }
    }

    pub(crate) fn grant(&self) -> MemoryGrant {
        self.grant.clone()
    }
    pub(crate) fn is_authorized(&self) -> bool {
        !self.grant.is_revoked()
    }
    pub(crate) fn check_connection(
        &self,
        db: &Connection,
        session: &str,
        changed: Option<(&str, &str, i64)>,
    ) -> Result<(), MemoryError> {
        check(db, &self.grant, false, session)?;
        if let Some((scope, entry, next)) = changed
            && scope == self.grant.scope
            && entry == self.value.entry_id
            && version(&self.value.version)?.checked_add(1) == Some(next)
        {
            let valid:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM memory_entries WHERE scope_id=? AND entry_id=? AND version=?)",params![scope,entry,next],|r|r.get(0))?;
            if valid {
                return Ok(());
            }
        }
        let current:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM memory_entries WHERE scope_id=? AND entry_id=? AND version=? AND (state='active' OR (?=1 AND state='disabled')))",params![self.grant.scope,self.value.entry_id,version(&self.value.version)?,self.include_disabled],|r|r.get(0))?;
        if !current {
            return Err(MemoryError::Conflict);
        }
        Ok(())
    }
    pub fn view<'a>(&'a self, journal: &'a Journal) -> Result<&'a MemoryValue, MemoryError> {
        self.check_connection(&journal.connection, &journal.memory_session, None)?;
        Ok(&self.value)
    }
}

impl Journal {
    pub fn read_memory(
        &self,
        grant: &MemoryGrant,
        id: &str,
        expected: Option<&str>,
        start: u32,
        limit: u32,
        include_disabled: bool,
    ) -> Result<MemoryReadReceipt, MemoryError> {
        let receipt =
            self.read_memory_for_agent(grant, id, expected, start, limit, include_disabled)?;
        self.require_source_free_memory(&grant.scope, id, &receipt.value.version)?;
        Ok(receipt)
    }
    pub(crate) fn read_memory_for_agent(
        &self,
        grant: &MemoryGrant,
        id: &str,
        expected: Option<&str>,
        start: u32,
        limit: u32,
        include_disabled: bool,
    ) -> Result<MemoryReadReceipt, MemoryError> {
        if !valid_id(id) || limit == 0 || limit > 4096 {
            return Err(MemoryError::Invalid);
        }
        check(&self.connection, grant, false, &self.memory_session)?;
        type Row = (i64, String, String, String, String, String, String);
        let row:Option<Row>=self.connection.query_row("SELECT e.version,e.payload,e.payload_digest,e.origin,e.source_actor,e.source_run,s.owner FROM memory_entries e JOIN memory_scopes s USING(scope_id) WHERE e.scope_id=? AND e.entry_id=? AND (e.state='active' OR (?=1 AND e.state='disabled'))",params![grant.scope,id,include_disabled],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let (revision, raw, expected_digest, origin, actor, run, owner) =
            row.ok_or(MemoryError::Denied)?;
        if raw.len() > MAX_PAYLOAD {
            return Err(MemoryError::Limit);
        }
        let payload: MemoryPayload =
            serde_json::from_str(&raw).map_err(|_| MemoryError::Invalid)?;
        payload.validate()?;
        if digest("memory-payload.v1", &payload)? != expected_digest
            || expected
                .map(version)
                .transpose()?
                .is_some_and(|v| v != revision)
        {
            return Err(MemoryError::Conflict);
        }
        let start = start as usize;
        if start > payload.content.len() || !payload.content.is_char_boundary(start) {
            return Err(MemoryError::Invalid);
        }
        let mut end = (start + limit as usize).min(payload.content.len());
        while !payload.content.is_char_boundary(end) {
            end -= 1;
        }
        if end == start && start < payload.content.len() {
            return Err(MemoryError::Invalid);
        }
        let origin = match origin.as_str() {
            "user_declaration" => MemoryOrigin::UserDeclaration,
            "model_inference" => MemoryOrigin::ModelInference,
            _ => return Err(MemoryError::Invalid),
        };
        let value = MemoryValue {
            scope_id: grant.scope.clone(),
            entry_id: id.into(),
            owner,
            version: revision.to_string(),
            title: payload.title,
            content: payload.content[start..end].into(),
            enabled: payload.enabled,
            origin,
            source_actor: actor,
            source_run: run,
            total_bytes: payload.content.len().to_string(),
            start_byte: start.to_string(),
            next_byte: (end < payload.content.len()).then(|| end.to_string()),
        };
        let receipt = MemoryReadReceipt {
            _lease: grant.lease()?,
            grant: grant.clone(),
            value,
            include_disabled,
        };
        receipt.view(self)?;
        Ok(receipt)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySummary {
    pub entry_id: String,
    pub version: String,
    pub title: String,
    pub enabled: bool,
    pub origin: MemoryOrigin,
    pub content_bytes: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPage {
    pub scope_id: String,
    pub entries: Vec<MemorySummary>,
    pub next_cursor: Option<String>,
}
pub struct MemoryListReceipt {
    _lease: MemoryLease,
    grant: MemoryGrant,
    page: MemoryPage,
    include_disabled: bool,
}
impl MemoryListReceipt {
    pub fn view<'a>(&'a self, journal: &'a Journal) -> Result<&'a MemoryPage, MemoryError> {
        check(
            &journal.connection,
            &self.grant,
            false,
            &journal.memory_session,
        )?;
        for value in &self.page.entries {
            let current:bool=journal.connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_entries WHERE scope_id=? AND entry_id=? AND version=? AND (state='active' OR (?=1 AND state='disabled')))",params![self.grant.scope,value.entry_id,version(&value.version)?,self.include_disabled],|r|r.get(0))?;
            if !current {
                return Err(MemoryError::Conflict);
            }
        }
        Ok(&self.page)
    }
}
impl Journal {
    pub fn list_memory(
        &self,
        grant: &MemoryGrant,
        after: Option<&str>,
        limit: u16,
        include_disabled: bool,
    ) -> Result<MemoryListReceipt, MemoryError> {
        if after.is_some_and(|v| !valid_id(v)) || !(1..=100).contains(&limit) {
            return Err(MemoryError::Invalid);
        }
        check(&self.connection, grant, false, &self.memory_session)?;
        let mut statement=self.connection.prepare("SELECT entry_id,version,payload,payload_digest,origin FROM memory_entries WHERE scope_id=?1 AND (?2 IS NULL OR entry_id>?2) AND (state='active' OR (?3=1 AND state='disabled')) ORDER BY entry_id LIMIT ?4")?;
        type Row = (String, i64, String, String, String);
        let rows = statement
            .query_map(
                params![grant.scope, after, include_disabled, i64::from(limit) + 1],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?
            .collect::<Result<Vec<Row>, _>>()?;
        let more = rows.len() > usize::from(limit);
        let mut entries = Vec::new();
        for (id, revision, body, expected, origin) in rows.into_iter().take(usize::from(limit)) {
            if body.len() > MAX_PAYLOAD {
                return Err(MemoryError::Limit);
            }
            let payload: MemoryPayload =
                serde_json::from_str(&body).map_err(|_| MemoryError::Invalid)?;
            payload.validate()?;
            if digest("memory-payload.v1", &payload)? != expected {
                return Err(MemoryError::Invalid);
            }
            let origin = match origin.as_str() {
                "user_declaration" => MemoryOrigin::UserDeclaration,
                "model_inference" => MemoryOrigin::ModelInference,
                _ => return Err(MemoryError::Invalid),
            };
            entries.push(MemorySummary {
                entry_id: id,
                version: revision.to_string(),
                title: payload.title,
                enabled: payload.enabled,
                origin,
                content_bytes: payload.content.len().to_string(),
            });
        }
        for entry in &entries {
            self.require_source_free_memory(&grant.scope, &entry.entry_id, &entry.version)?;
        }
        let next_cursor = if more {
            entries.last().map(|v| v.entry_id.clone())
        } else {
            None
        };
        let receipt = MemoryListReceipt {
            _lease: grant.lease()?,
            grant: grant.clone(),
            page: MemoryPage {
                scope_id: grant.scope.clone(),
                entries,
                next_cursor,
            },
            include_disabled,
        };
        receipt.view(self)?;
        Ok(receipt)
    }
    pub fn list_memory_drafts(
        &self,
        grant: &MemoryGrant,
        before: Option<&str>,
        limit: u16,
    ) -> Result<Vec<MemoryDraftOutcome>, MemoryError> {
        if before.is_some_and(|v| !valid_id(v)) || !(1..=100).contains(&limit) {
            return Err(MemoryError::Invalid);
        }
        check(&self.connection, grant, false, &self.memory_session)?;
        let mut statement=self.connection.prepare("SELECT draft_id FROM memory_drafts WHERE scope_id=?1 AND actor=?2 AND (?3 IS NULL OR draft_id<?3) ORDER BY draft_id DESC LIMIT ?4")?;
        let ids = statement
            .query_map(
                params![grant.scope, grant.actor, before, i64::from(limit)],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| {
                self.memory_draft_outcome(grant, id)?
                    .ok_or(MemoryError::Conflict)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryCapability {
    pub scope_id: String,
    pub revision: String,
    pub write: bool,
    pub environment: String,
}
impl Journal {
    pub fn memory_capability(
        &self,
        grant: &MemoryGrant,
        actor: &str,
        policy: &str,
    ) -> Result<MemoryCapability, MemoryError> {
        if grant.actor != actor || grant.policy != policy {
            return Err(MemoryError::Denied);
        }
        check(&self.connection, grant, grant.write, &self.memory_session)?;
        Ok(MemoryCapability {
            scope_id: grant.scope.clone(),
            revision: grant.revision.to_string(),
            write: grant.write,
            environment: digest(
                "memory-environment.v1",
                &(actor, policy, &grant.scope, grant.revision, grant.write),
            )?,
        })
    }
}

pub(crate) fn check_execution_grants(
    db: &Connection,
    grants: &[MemoryGrant],
    actor: &str,
    policy: &str,
    session: &str,
) -> Result<(), MemoryError> {
    for grant in grants {
        if grant.actor != actor || grant.policy != policy {
            return Err(MemoryError::Denied);
        }
        check(db, grant, grant.write, session)?;
    }
    Ok(())
}

impl Journal {
    fn require_source_free_memory(
        &self,
        scope: &str,
        entry: &str,
        version: &str,
    ) -> Result<(), MemoryError> {
        let (origin,run,actor):(String,String,String)=self.connection.query_row("SELECT origin,source_run,source_actor FROM memory_entries WHERE scope_id=? AND entry_id=? AND version=?",params![scope,entry,version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let continuity = self.memory_continuity(scope, entry, version)?;
        if continuity.root_origin.name() != origin
            || continuity.root_source_run != run
            || continuity.root_actor != actor
        {
            return Err(MemoryError::Invalid);
        }
        let Some((ancestor, run)) = continuity.model_ancestor else {
            return Ok(());
        };
        let version = ancestor.as_str();
        type Row = (String, String, String, String, String, String);
        let row:Option<Row>=self.connection.query_row("SELECT l.body,l.digest,i.request_digest,m.operation_id,i.request,o.run_id FROM memory_drafts d JOIN runtime_memory_operations m USING(draft_id) JOIN runtime_memory_lineage l USING(operation_id) JOIN operations o USING(operation_id) JOIN runtime_invocations i USING(invocation) WHERE d.scope_id=? AND d.entry_id=? AND d.result_version=? AND d.state='approved'",params![scope,entry,version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        if let Some((body, sha, invocation, operation, request, run)) = row {
            if body.len() > 64 * 1024 {
                return Err(MemoryError::Limit);
            }
            let graph: serde_json::Value =
                serde_json::from_str(&body).map_err(|_| MemoryError::Invalid)?;
            if digest("memory-provenance.v1", &graph)? != sha
                || graph.as_object().is_none_or(|o| o.len() != 3)
                || graph["operation_id"] != operation
                || graph["invocation_digest"] != invocation
            {
                return Err(MemoryError::Invalid);
            }
            if graph["sources"].as_array().is_some_and(Vec::is_empty) {
                if request.len() > 2 * 1024 * 1024 {
                    return Err(MemoryError::Limit);
                }
                let request: orange_protocol::model::GenerateRequest =
                    serde_json::from_str(&request).map_err(|_| MemoryError::Invalid)?;
                if digest("model-request-v1", &request)? != invocation {
                    return Err(MemoryError::Invalid);
                }
                for message in request
                    .messages
                    .iter()
                    .filter(|v| v.role == orange_protocol::model::Role::Tool)
                {
                    let call = message
                        .tool_call_id
                        .as_deref()
                        .ok_or(MemoryError::Invalid)?;
                    let row:Option<(String,Option<String>,String)>=self.connection.query_row("SELECT state,model_result,request_json FROM operations WHERE run_id=? AND call_id=?",params![run,call],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                    if let Some((state, result, request)) = row {
                        if result.as_deref() != Some(&message.content) {
                            return Err(MemoryError::Invalid);
                        }
                        let data: serde_json::Value = serde_json::from_str(&message.content)
                            .map_err(|_| MemoryError::Invalid)?;
                        let request: serde_json::Value =
                            serde_json::from_str(&request).map_err(|_| MemoryError::Invalid)?;
                        let no_data = state == "failed"
                            && data.as_object().is_some_and(|o| {
                                o.len() == 1
                                    && o.get("error").is_some_and(serde_json::Value::is_string)
                            });
                        let deletion = state == "succeeded"
                            && request["tool"] == "memory_write"
                            && request["arguments"]["mutation"] == "delete";
                        if !no_data && !deletion {
                            return Err(MemoryError::SourceRequired);
                        }
                    } else {
                        let answers:Option<String>=self.connection.query_row("SELECT answers FROM runtime_questions WHERE run_id=? AND call_id=? AND state='answered'",params![run,call],|r|r.get(0)).optional()?;
                        let answers: Vec<String> =
                            serde_json::from_str(&answers.ok_or(MemoryError::SourceRequired)?)
                                .map_err(|_| MemoryError::Invalid)?;
                        if encoded(&serde_json::json!({"answers":answers}))? != message.content {
                            return Err(MemoryError::Invalid);
                        }
                    }
                }
                return Ok(());
            }
        } else {
            let derived:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?1 UNION SELECT 1 FROM operations WHERE run_id=?1)",[run],|r|r.get(0))?;
            if !derived {
                return Ok(());
            }
        }
        Err(MemoryError::SourceRequired)
    }
}

impl Journal {
    fn memory_scope_signal(&self, scope: &str, revision: i64) -> Arc<ScopeSignal> {
        let mut signals = self.memory_signals.borrow_mut();
        signals.retain(|_, v| v.strong_count() > 0);
        let key = (scope.to_owned(), revision);
        if let Some(signal) = signals.get(&key).and_then(std::sync::Weak::upgrade) {
            return signal;
        }
        let signal = Arc::new(ScopeSignal {
            closed: AtomicBool::new(false),
            notice: tokio::sync::watch::channel(false).0,
            gate: Arc::new(tokio::sync::RwLock::new(())),
        });
        signals.insert(key, Arc::downgrade(&signal));
        signal
    }
    /// Cache this trusted-owner revoker before queuing a scope mutation behind
    /// active work. Signal first, then await real drain before acknowledging.
    pub fn memory_scope_revoker(
        &self,
        owner: &str,
        scope: &str,
        expected: &str,
    ) -> Result<MemoryScopeRevoker, MemoryError> {
        let value = self.memory_scope(owner, scope)?;
        if value.owner != owner || value.revision != expected {
            return Err(MemoryError::Denied);
        }
        let revision = version(expected)?;
        let current = self.memory_scope_signal(scope, revision);
        let mut signals = self
            .memory_signals
            .borrow()
            .iter()
            .filter(|((id, rev), _)| id == scope && *rev <= revision)
            .filter_map(|(_, value)| value.upgrade())
            .collect::<Vec<_>>();
        if !signals.iter().any(|s| Arc::ptr_eq(s, &current)) {
            signals.push(current);
        }
        Ok(MemoryScopeRevoker { signals })
    }
}

#[derive(Clone)]
pub(crate) struct ManagementCursor {
    pub actor: String,
    pub scope: String,
    pub environment: String,
    pub after: String,
    pub include_disabled: bool,
    pub expires: std::time::Instant,
}
