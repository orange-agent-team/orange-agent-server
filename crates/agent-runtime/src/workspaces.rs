//! Durable workspace consent for trusted hosts. Model arguments cannot invoke
//! these methods; the caller supplies an authenticated/local actor and consent.
use crate::{
    Journal, JournalError,
    workspace::{WorkspaceError, WorkspaceGrant},
};
use orange_protocol::{request_digest, valid_id};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceStoreError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("invalid workspace command")]
    Invalid,
    #[error("workspace not found for actor")]
    NotFound,
    #[error("workspace permission is revoked")]
    Revoked,
    #[error("workspace identity or policy conflict")]
    Conflict,
    #[error("unresolved execution requires recovery")]
    RecoveryRequired,
    #[error("workspace inventory limit exceeded")]
    Limit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub workspace_id: String,
    pub canonical_path: String,
    pub resource_identity: String,
    pub environment: String,
    pub policy: String,
    pub active: bool,
    pub version: String,
}
/// Signal can stop in-flight permits before the host awaits the Journal lock.
/// Only a successful revoke_workspace commit acknowledges durable revocation.
#[derive(Clone)]
pub struct WorkspaceRevoker {
    flag: Arc<AtomicBool>,
}
impl WorkspaceRevoker {
    pub fn request_revoke(&self) {
        self.flag.store(true, Ordering::Release);
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub workspace_id: Option<String>,
    pub phase: String,
    pub steps: u32,
    pub invocations: u32,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    pub next_cursor: Option<String>,
}

impl Journal {
    fn protected_roots(&self) -> Vec<std::path::PathBuf> {
        let mut roots = vec![self.private_root.clone()];
        roots.extend(self.extra_private_roots.iter().cloned());
        roots
    }
    /// Trusted Host startup hook for private IPC/cache areas. Register before
    /// writing secrets or restoring workspace access. Never removes protection.
    pub fn reserve_private_directory(&mut self, path: &Path) -> Result<(), WorkspaceStoreError> {
        if path.is_symlink() {
            return Err(WorkspaceStoreError::Invalid);
        }
        let path = path.canonicalize().map_err(WorkspaceError::Io)?;
        let metadata = std::fs::metadata(&path).map_err(WorkspaceError::Io)?;
        if !metadata.is_dir() {
            return Err(WorkspaceStoreError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 {
                return Err(WorkspaceStoreError::Invalid);
            }
        }
        let mut roots = self.protected_roots();
        if roots.contains(&path) {
            return Ok(());
        }
        if roots.len() >= 64 {
            return Err(WorkspaceStoreError::Limit);
        }
        roots.push(path.clone());
        let legacy_active:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_runs rr JOIN runs r USING(run_id) LEFT JOIN run_workspaces rw USING(run_id) WHERE rw.workspace_id IS NULL AND r.active=1 AND rr.phase IN('ready','model_pending','tools_pending','awaiting_user','recovery_required'))",[],|r|r.get(0))?;
        if legacy_active {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let unresolved:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE state IN('unknown','dispatching') UNION SELECT 1 FROM runtime_invocations WHERE state IN('unknown','dispatched'))",[],|r|r.get(0))?;
        if unresolved {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let rows = {
            let mut query=self.connection.prepare("SELECT owner,workspace_id,policy,canonical_path,environment,resource_identity FROM local_workspaces WHERE active=1")?;
            query
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (owner, id, policy, root, environment, resource) in rows {
            match WorkspaceGrant::open(&owner, &id, &policy, Path::new(&root), &roots) {
                Ok(grant)
                    if grant.environment() == environment
                        && grant.resource_identity() == resource => {}
                _ => return Err(WorkspaceStoreError::Conflict),
            }
        }
        let mutation_rows = {
            let mut query=self.connection.prepare("SELECT owner,consent_id,policy,canonical_path,resource_identity FROM creation_consents WHERE active=1 UNION ALL SELECT owner,consent_id,policy,canonical_path,resource_identity FROM editing_consents WHERE active=1")?;
            query
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (owner, id, policy, root, resource) in mutation_rows {
            match WorkspaceGrant::open(&owner, &id, &policy, Path::new(&root), &roots) {
                Ok(grant) if grant.resource_identity() == resource => {}
                _ => return Err(WorkspaceStoreError::Conflict),
            }
        }
        self.extra_private_roots.push(path);
        Ok(())
    }
    pub fn workspace(&self, owner: &str, id: &str) -> Result<WorkspaceView, WorkspaceStoreError> {
        if !valid_id(owner) || !valid_id(id) {
            return Err(WorkspaceStoreError::Invalid);
        }
        self.connection.query_row("SELECT workspace_id,canonical_path,resource_identity,environment,policy,active,version FROM local_workspaces WHERE workspace_id=? AND owner=?",params![id,owner],workspace_row).optional()?.ok_or(WorkspaceStoreError::NotFound)
    }
    pub fn workspaces(
        &self,
        owner: &str,
        include_revoked: bool,
    ) -> Result<Vec<WorkspaceView>, WorkspaceStoreError> {
        if !valid_id(owner) {
            return Err(WorkspaceStoreError::Invalid);
        }
        let mut query=self.connection.prepare("SELECT workspace_id,canonical_path,resource_identity,environment,policy,active,version FROM local_workspaces WHERE owner=? AND (active=1 OR ?) ORDER BY rowid DESC LIMIT 1000")?;
        Ok(query
            .query_map(params![owner, include_revoked], workspace_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn authorize_workspace(
        &mut self,
        owner: &str,
        request_id: &str,
        root: &Path,
        policy: &str,
    ) -> Result<WorkspaceView, WorkspaceStoreError> {
        if ![owner, request_id, policy].iter().all(|v| valid_id(v)) {
            return Err(WorkspaceStoreError::Invalid);
        }
        let canonical = root.canonicalize().map_err(WorkspaceError::Io)?;
        let path = canonical
            .to_str()
            .filter(|s| s.len() <= 4096)
            .ok_or(WorkspaceStoreError::Invalid)?;
        let id = format!("workspace-{}", uuid::Uuid::new_v4());
        let probe = WorkspaceGrant::open(owner, &id, policy, &canonical, &self.protected_roots())?;
        if !probe.has_persistent_identity() {
            return Err(WorkspaceError::PersistentIdentityUnavailable.into());
        }
        let digest = request_digest(
            "workspace-consent-v1",
            &(owner, path, policy, probe.resource_identity()),
        )
        .map_err(|_| WorkspaceStoreError::Invalid)?;
        let old:Option<(String,String)>=self.connection.query_row("SELECT digest,workspace_id FROM workspace_request_keys WHERE owner=? AND request_id=?",params![owner,request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((expected, workspace)) = old {
            if expected != digest {
                return Err(WorkspaceStoreError::Conflict);
            }
            let view = self.workspace(owner, &workspace)?;
            if !view.active {
                return Err(WorkspaceStoreError::Revoked);
            }
            self.validate_workspace_root(owner, &view)?;
            return Ok(view);
        }
        let existing:Option<String>=self.connection.query_row("SELECT workspace_id FROM local_workspaces WHERE owner=? AND canonical_path=? AND active=1",params![owner,path],|r|r.get(0)).optional()?;
        if let Some(existing) = existing {
            let view = self.workspace(owner, &existing)?;
            if view.resource_identity != probe.resource_identity() {
                self.revoke_workspace(owner, &existing, &view.version)?;
                return Err(WorkspaceError::EnvironmentChanged.into());
            }
            if view.policy != policy {
                return Err(WorkspaceStoreError::Conflict);
            }
            self.validate_workspace_root(owner, &view)?;
            self.connection.execute("INSERT INTO workspace_request_keys(owner,request_id,digest,workspace_id) VALUES(?,?,?,?)",params![owner,request_id,digest,existing])?;
            return Ok(view);
        }
        if quarantined(&self.connection, owner, probe.resource_identity())? {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM local_workspaces WHERE owner=?",
            [owner],
            |r| r.get(0),
        )?;
        let active: i64 = tx.query_row(
            "SELECT count(*) FROM local_workspaces WHERE owner=? AND active=1",
            [owner],
            |r| r.get(0),
        )?;
        if count >= 1000 || active >= 32 {
            return Err(WorkspaceStoreError::Limit);
        }
        tx.execute("INSERT INTO local_workspaces(workspace_id,owner,canonical_path,resource_identity,environment,policy,active,version) VALUES(?,?,?,?,?,?,1,1)",params![id,owner,path,probe.resource_identity(),probe.environment(),policy])?;
        tx.execute("INSERT INTO workspace_request_keys(owner,request_id,digest,workspace_id) VALUES(?,?,?,?)",params![owner,request_id,digest,id])?;
        tx.execute("INSERT INTO workspace_audit(workspace_id,owner,kind,version) VALUES(?,?,'workspace.authorized',1)",params![id,owner])?;
        tx.commit()?;
        self.workspace(owner, &id)
    }
    fn validate_workspace_root(
        &mut self,
        owner: &str,
        view: &WorkspaceView,
    ) -> Result<(), WorkspaceStoreError> {
        if !view.active {
            return Err(WorkspaceStoreError::Revoked);
        }
        let result = WorkspaceGrant::open(
            owner,
            &view.workspace_id,
            &view.policy,
            Path::new(&view.canonical_path),
            &self.protected_roots(),
        );
        match result {
            Ok(grant)
                if grant.has_persistent_identity()
                    && grant.environment() == view.environment
                    && grant.resource_identity() == view.resource_identity =>
            {
                Ok(())
            }
            _ => {
                self.revoke_workspace(owner, &view.workspace_id, &view.version)?;
                Err(WorkspaceError::EnvironmentChanged.into())
            }
        }
    }
    pub fn workspace_revoker(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<WorkspaceRevoker, WorkspaceStoreError> {
        let view = self.workspace(owner, id)?;
        if !view.active {
            return Err(WorkspaceStoreError::Revoked);
        }
        let flag = self
            .workspace_signals
            .entry(id.into())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        Ok(WorkspaceRevoker { flag })
    }
    pub fn open_workspace(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<WorkspaceGrant, WorkspaceStoreError> {
        let view = self.workspace(owner, id)?;
        self.validate_workspace_root(owner, &view)?;
        if quarantined(&self.connection, owner, &view.resource_identity)? {
            return Err(WorkspaceStoreError::RecoveryRequired);
        }
        let revoker = self.workspace_revoker(owner, id)?;
        if revoker.flag.load(Ordering::Acquire) {
            return Err(WorkspaceStoreError::Revoked);
        }
        let grant = WorkspaceGrant::open_scoped(
            owner,
            id,
            &view.policy,
            Path::new(&view.canonical_path),
            &self.protected_roots(),
            Some(revoker.flag),
        )?;
        // Verify again after opening the capability: a path swap between the two
        // opens must not change which object receives the persisted authority.
        if grant.environment() != view.environment
            || grant.resource_identity() != view.resource_identity
        {
            self.revoke_workspace(owner, id, &view.version)?;
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        Ok(grant)
    }
    pub fn revoke_workspace(
        &mut self,
        owner: &str,
        id: &str,
        expected_version: &str,
    ) -> Result<WorkspaceView, WorkspaceStoreError> {
        let expected = parse_counter(expected_version)?;
        let view = self.workspace(owner, id)?;
        if !view.active {
            return Ok(view);
        }
        if view.version != expected.to_string() {
            return Err(WorkspaceStoreError::Conflict);
        }
        self.workspace_revoker(owner, id)?.request_revoke();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE local_workspaces SET active=0,version=version+1 WHERE workspace_id=? AND owner=? AND version=? AND active=1",params![id,owner,expected])?!=1{return Err(WorkspaceStoreError::Conflict);}
        tx.execute("UPDATE runs SET active=0 WHERE run_id IN(SELECT run_id FROM run_workspaces WHERE workspace_id=?)",[id])?;
        tx.execute("UPDATE operations SET state='unknown' WHERE state='dispatching' AND run_id IN(SELECT run_id FROM run_workspaces WHERE workspace_id=?)",[id])?;
        tx.execute("UPDATE runtime_invocations SET state='unknown' WHERE state='dispatched' AND run_id IN(SELECT run_id FROM run_workspaces WHERE workspace_id=?)",[id])?;
        tx.execute("UPDATE runtime_runs SET phase='cancelled',failure='workspace_revoked' WHERE phase!='completed' AND run_id IN(SELECT run_id FROM run_workspaces WHERE workspace_id=?)",[id])?;
        tx.execute("UPDATE runtime_questions SET state='cancelled' WHERE state='pending' AND run_id IN(SELECT run_id FROM run_workspaces WHERE workspace_id=?)",[id])?;
        tx.execute("INSERT INTO workspace_audit(workspace_id,owner,kind,version) SELECT workspace_id,owner,'workspace.revoked',version FROM local_workspaces WHERE workspace_id=?",[id])?;
        tx.commit()?;
        self.workspace(owner, id)
    }
    /// Status-only pagination: no prompts, tool content or final answer is
    /// re-exposed just because the UI reconnects to an event/history list.
    pub fn list_agent_runs(
        &self,
        owner: &str,
        workspace_id: Option<&str>,
        before: Option<&str>,
        limit: u16,
    ) -> Result<RunPage, WorkspaceStoreError> {
        if !valid_id(owner) || !(1..=100).contains(&limit) {
            return Err(WorkspaceStoreError::Invalid);
        }
        if let Some(id) = workspace_id {
            self.workspace(owner, id)?;
        }
        let before = before.map(parse_counter).transpose()?.unwrap_or(i64::MAX);
        let mut query=self.connection.prepare("SELECT rr.rowid,rr.run_id,rw.workspace_id,rr.phase,rr.steps,rr.invocations,r.active FROM runtime_runs rr JOIN runs r USING(run_id) LEFT JOIN run_workspaces rw USING(run_id) WHERE r.owner=?1 AND rr.rowid<?2 AND (?3 IS NULL OR rw.workspace_id=?3) ORDER BY rr.rowid DESC LIMIT ?4")?;
        let mut rows = query
            .query_map(
                params![owner, before, workspace_id, u32::from(limit) + 1],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        RunSummary {
                            run_id: r.get(1)?,
                            workspace_id: r.get(2)?,
                            phase: r.get(3)?,
                            steps: r.get(4)?,
                            invocations: r.get(5)?,
                            active: r.get(6)?,
                        },
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let next_cursor = if more {
            rows.last().map(|(id, _)| id.to_string())
        } else {
            None
        };
        Ok(RunPage {
            runs: rows.into_iter().map(|(_, view)| view).collect(),
            next_cursor,
        })
    }
}
fn workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceView> {
    Ok(WorkspaceView {
        workspace_id: row.get(0)?,
        canonical_path: row.get(1)?,
        resource_identity: row.get(2)?,
        environment: row.get(3)?,
        policy: row.get(4)?,
        active: row.get(5)?,
        version: row.get::<_, i64>(6)?.to_string(),
    })
}
fn parse_counter(value: &str) -> Result<i64, WorkspaceStoreError> {
    if value.is_empty()
        || value.len() > 19
        || !value.bytes().all(|c| c.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(WorkspaceStoreError::Invalid);
    }
    value
        .parse::<i64>()
        .map_err(|_| WorkspaceStoreError::Invalid)
}
pub(crate) fn quarantined(
    connection: &rusqlite::Connection,
    owner: &str,
    resource: &str,
) -> Result<bool, rusqlite::Error> {
    connection.query_row("SELECT EXISTS(
      SELECT 1 FROM operations o LEFT JOIN run_workspaces rw USING(run_id) LEFT JOIN local_workspaces w USING(workspace_id) WHERE o.owner=?1 AND o.state IN('unknown','dispatching') AND (w.resource_identity=?2 OR rw.workspace_id IS NULL)
      UNION SELECT 1 FROM runtime_invocations i JOIN runs r USING(run_id) LEFT JOIN run_workspaces rw USING(run_id) LEFT JOIN local_workspaces w USING(workspace_id) WHERE r.owner=?1 AND i.state IN('unknown','dispatched') AND (w.resource_identity=?2 OR rw.workspace_id IS NULL)
      UNION SELECT 1 FROM operations o JOIN editing_consents c ON c.environment=o.environment WHERE c.owner=?1 AND o.owner=?1 AND c.resource_identity=?2 AND o.state IN('unknown','dispatching')
      UNION SELECT 1 FROM runtime_runs rr JOIN runs r USING(run_id) LEFT JOIN run_workspaces rw USING(run_id) LEFT JOIN local_workspaces w USING(workspace_id) WHERE r.owner=?1 AND rr.phase='recovery_required' AND (w.resource_identity=?2 OR rw.workspace_id IS NULL))",params![owner,resource],|r|r.get(0))
}

impl Drop for Journal {
    fn drop(&mut self) {
        for flag in self.workspace_signals.values() {
            flag.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests;
