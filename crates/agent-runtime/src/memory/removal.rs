//! Scope-owner data removal without opening a content or execution capability.
use super::*;

pub(super) fn tombstone(
    db: &Connection,
    scope: &str,
    entry: &str,
    next: i64,
    actor: &str,
    source: &str,
    origin: MemoryOrigin,
) -> Result<(), MemoryError> {
    db.execute("INSERT OR IGNORE INTO memory_source_blocks SELECT scope_id,json_extract(body,'$.source_run') FROM memory_drafts WHERE scope_id=? AND entry_id=?",params![scope,entry])?;
    db.execute("INSERT OR IGNORE INTO memory_source_blocks SELECT scope_id,source_run FROM memory_entries WHERE scope_id=? AND entry_id=?",params![scope,entry])?;
    let sha = digest("memory-deletion.v1", &(scope, entry, next))?;
    if db.execute("UPDATE memory_entries SET version=?,state='deleted',payload=NULL,payload_digest=?,source_actor=?,source_run=?,origin=? WHERE scope_id=? AND entry_id=? AND version=? AND state!='deleted'",params![next,sha,actor,source,origin.name(),scope,entry,next-1])?!=1 {return Err(MemoryError::Conflict);}
    Ok(())
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OwnerMemoryEntry {
    pub entry_id: String,
    pub version: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OwnerRemovalTarget {
    pub scope_id: String,
    pub scope_revision: String,
    pub entry_id: String,
    pub entry_version: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct OwnerRemovalReview {
    pub request_id: String,
    pub target: OwnerRemovalTarget,
    pub expires_at: i64,
    pub approval_digest: String,
    pub applied: bool,
}
fn owner_revision(db: &Connection, owner: &str, scope: &str) -> Result<i64, MemoryError> {
    if !valid_id(owner) || !valid_id(scope) {
        return Err(MemoryError::Invalid);
    }
    db.query_row(
        "SELECT revision FROM memory_scopes WHERE owner=? AND scope_id=?",
        params![owner, scope],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(MemoryError::Denied)
}
fn receipt(
    db: &Connection,
    owner: &str,
    scope: &str,
    key: &str,
) -> Result<Option<OwnerRemovalReview>, MemoryError> {
    let value=db.query_row("SELECT scope_revision,entry_id,entry_version,expires_at,digest,applied FROM memory_owner_removals WHERE actor=? AND scope_id=? AND request_id=?",params![owner,scope,key],|row|Ok(OwnerRemovalReview{
        request_id:key.to_owned(),target:OwnerRemovalTarget{scope_id:scope.to_owned(),scope_revision:row.get::<_,i64>(0)?.to_string(),entry_id:row.get(1)?,entry_version:row.get::<_,i64>(2)?.to_string()},expires_at:row.get(3)?,approval_digest:row.get(4)?,applied:row.get(5)?
    })).optional()?;
    if let Some(r) = &value
        && r.approval_digest
            != digest(
                "owner-memory-removal.v1",
                &(owner, key, &r.target, r.expires_at),
            )?
    {
        return Err(MemoryError::Invalid);
    }
    Ok(value)
}
impl Journal {
    pub fn owner_memory_inventory(
        &self,
        owner: &str,
        scope: &str,
        expected_revision: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<OwnerMemoryEntry>, MemoryError> {
        if after.is_some_and(|s| !valid_id(s)) || !(1..=100).contains(&limit) {
            return Err(MemoryError::Invalid);
        }
        if owner_revision(&self.connection, owner, scope)? != version(expected_revision)? {
            return Err(MemoryError::Conflict);
        }
        let mut q=self.connection.prepare("SELECT entry_id,version,state='active' FROM memory_entries WHERE scope_id=?1 AND state!='deleted' AND (?2 IS NULL OR entry_id>?2) ORDER BY entry_id LIMIT ?3")?;
        Ok(q.query_map(params![scope, after, limit], |r| {
            Ok(OwnerMemoryEntry {
                entry_id: r.get(0)?,
                version: r.get::<_, i64>(1)?.to_string(),
                enabled: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn inspect_owner_removal(
        &self,
        owner: &str,
        scope: &str,
        key: &str,
    ) -> Result<Option<OwnerRemovalReview>, MemoryError> {
        owner_revision(&self.connection, owner, scope)?;
        if !valid_id(key) {
            return Err(MemoryError::Invalid);
        }
        receipt(&self.connection, owner, scope, key)
    }
    pub fn prepare_owner_removal(
        &mut self,
        owner: &str,
        key: &str,
        target: OwnerRemovalTarget,
    ) -> Result<OwnerRemovalReview, MemoryError> {
        if !valid_id(key) || !valid_id(&target.entry_id) {
            return Err(MemoryError::Invalid);
        }
        let revision = version(&target.scope_revision)?;
        let entry_version = version(&target.entry_version)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = owner_revision(&tx, owner, &target.scope_id)?;
        let time = now()?;
        let entry:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_entries WHERE scope_id=? AND entry_id=? AND version=? AND state!='deleted')",params![target.scope_id,target.entry_id,entry_version],|r|r.get(0))?;
        if let Some(old) = receipt(&tx, owner, &target.scope_id, key)? {
            if old.target != target {
                return Err(MemoryError::Conflict);
            }
            if !old.applied && (old.expires_at <= time || revision != current || !entry) {
                return Err(MemoryError::ApprovalRequired);
            }
            return Ok(old);
        }
        if current != revision || !entry {
            return Err(MemoryError::Conflict);
        }
        tx.execute("DELETE FROM memory_owner_removals WHERE scope_id=? AND applied=0 AND (expires_at<=? OR scope_revision!=?)",params![target.scope_id,time,revision])?;
        let pending: i64 = tx.query_row(
            "SELECT count(*) FROM memory_owner_removals WHERE scope_id=? AND applied=0",
            [&target.scope_id],
            |r| r.get(0),
        )?;
        if pending >= 128 {
            return Err(MemoryError::Limit);
        }
        let expires = time.checked_add(300).ok_or(MemoryError::Limit)?;
        let sha = digest("owner-memory-removal.v1", &(owner, key, &target, expires))?;
        tx.execute(
            "INSERT INTO memory_owner_removals VALUES(?,?,?,?,?,?,?,?,0)",
            params![
                target.scope_id,
                key,
                owner,
                revision,
                target.entry_id,
                entry_version,
                expires,
                sha
            ],
        )?;
        tx.commit()?;
        Ok(OwnerRemovalReview {
            request_id: key.into(),
            target,
            expires_at: expires,
            approval_digest: sha,
            applied: false,
        })
    }
    /// The acknowledgement waits for old scope readers and effect workers.
    pub async fn approve_owner_removal(
        &mut self,
        owner: &str,
        scope: &str,
        key: &str,
        expected: &str,
    ) -> Result<OwnerRemovalReview, MemoryError> {
        let original = self
            .inspect_owner_removal(owner, scope, key)?
            .ok_or(MemoryError::ApprovalRequired)?;
        if original.approval_digest != expected {
            return Err(MemoryError::ApprovalRequired);
        }
        let rev = version(&original.target.scope_revision)?;
        let current = owner_revision(&self.connection, owner, scope)?;
        // A retry after a dropped acknowledgement must also drain the old leases.
        if original.applied {
            self.wait_prior_memory_leases(scope, rev).await;
            return Ok(original);
        }
        if current != rev || original.expires_at <= now()? {
            return Err(MemoryError::ApprovalRequired);
        }
        let old = self.memory_scope_revoker(owner, scope, &current.to_string())?;
        let next_revision = rev.checked_add(1).ok_or(MemoryError::Limit)?;
        let next_version = version(&original.target.entry_version)?
            .checked_add(1)
            .ok_or(MemoryError::Limit)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = format!("removal-{}", uuid::Uuid::new_v4());
        tx.execute(
            "INSERT INTO runs(run_id,owner,policy,active) VALUES(?,?,'owner-memory-removal.v1',0)",
            params![source, owner],
        )?;
        tombstone(
            &tx,
            scope,
            &original.target.entry_id,
            next_version,
            owner,
            &source,
            MemoryOrigin::UserDeclaration,
        )?;
        if tx.execute(
            "UPDATE memory_scopes SET revision=? WHERE scope_id=? AND owner=? AND revision=?",
            params![next_revision, scope, owner, rev],
        )? != 1
        {
            return Err(MemoryError::Conflict);
        }
        tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE scope_id=? AND state='pending'",[scope])?;
        if tx.execute("UPDATE memory_owner_removals SET applied=1 WHERE scope_id=? AND request_id=? AND digest=? AND applied=0 AND expires_at>?",params![scope,key,expected,now()?])?!=1{return Err(MemoryError::ApprovalRequired);}
        tx.execute("INSERT INTO memory_audit(scope_id,actor,action,entry_id,revision,digest) VALUES(?,?,'owner_entry_removed',?,?,?)",params![scope,owner,original.target.entry_id,next_version,expected])?;
        tx.commit()?;
        old.request_revoke();
        old.wait_drained().await;
        Ok(OwnerRemovalReview {
            applied: true,
            ..original
        })
    }
    async fn wait_prior_memory_leases(&self, scope: &str, max_revision: i64) {
        let signals = self
            .memory_signals
            .borrow()
            .iter()
            .filter(|((id, rev), _)| id == scope && *rev <= max_revision)
            .filter_map(|(_, v)| v.upgrade())
            .collect();
        MemoryScopeRevoker { signals }.wait_drained().await;
    }
}
