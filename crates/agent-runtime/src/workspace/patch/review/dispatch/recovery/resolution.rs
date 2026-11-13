//! Explicit recovery authority, distinct from ordinary reads, file edits and
//! model approvals. An unknown publication is never resumed automatically.
use super::*;
use crate::workspace::Permit;
use inspection::{PatchRecoveryReceipt, PatchRecoverySnapshot, RecoveryFileObservation};
use std::path::Path;
mod publish;
pub use publish::PatchResolutionResult;
#[cfg(test)]
mod tests;

pub struct PatchRecoveryGrant {
    grant: WorkspaceGrant,
    operation: String,
    environment: String,
    epoch: i64,
}
impl PatchRecoveryGrant {
    pub fn request_revoke(&self) {
        self.grant.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.grant.wait_drained().await;
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatchRecoveryChoice {
    KeepCurrentTarget,
    RestoreOriginalToMissingTarget,
    PublishStagedToMissingTarget,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchResolutionReview {
    pub recovery_id: String,
    pub digest: String,
    pub choice: PatchRecoveryChoice,
    pub snapshot: PatchRecoverySnapshot,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionRecord {
    owner: String,
    operation: String,
    environment: String,
    read_environment: String,
    resource: String,
    policy: String,
    choice: PatchRecoveryChoice,
    snapshot: PatchRecoverySnapshot,
}
pub struct PatchResolutionDispatch {
    record: ResolutionRecord,
    review: PatchResolutionReview,
    read: PatchRecoveryReceipt,
    write: Permit,
}
impl PatchResolutionDispatch {
    pub fn review(&self) -> &PatchResolutionReview {
        &self.review
    }
    pub fn has_live_directory_access(&self) -> bool {
        !self.write.inner.is_revoked() && self.read.snapshot().is_ok()
    }
}
fn valid_choice(choice: PatchRecoveryChoice, snapshot: &PatchRecoverySnapshot) -> bool {
    if snapshot.journal.execution_state != "unknown" {
        return false;
    }
    match choice {
        PatchRecoveryChoice::KeepCurrentTarget => {
            matches!(snapshot.target, RecoveryFileObservation::Regular { .. })
        }
        PatchRecoveryChoice::RestoreOriginalToMissingTarget => {
            matches!(snapshot.target, RecoveryFileObservation::Missing)
                && matches!(
                    snapshot.original,
                    RecoveryFileObservation::Regular {
                        reviewed_original_inode: true,
                        ..
                    }
                )
        }
        PatchRecoveryChoice::PublishStagedToMissingTarget => {
            matches!(snapshot.target, RecoveryFileObservation::Missing)
                && matches!(
                    snapshot.staged,
                    RecoveryFileObservation::Regular {
                        matches_result_content: true,
                        ..
                    }
                )
        }
    }
}
impl Journal {
    /// Trusted host only after separate recovery/write directory consent.
    /// Fresh nonce and Host epoch prevent revoked/reopened grants reviving an
    /// old approval. No conversion from read/create/edit capabilities exists.
    pub fn authorize_patch_recovery(
        &self,
        owner: &str,
        policy: &str,
        operation: &str,
        root: &Path,
    ) -> Result<PatchRecoveryGrant, PatchError> {
        if !valid_id(operation) {
            return Err(PatchError::Invalid);
        }
        let mut private_roots = self.extra_private_roots.clone();
        private_roots.push(self.private_root.clone());
        let consent = uuid::Uuid::new_v4().to_string();
        let grant = WorkspaceGrant::open(owner, &consent, policy, root, &private_roots)?;
        if !grant.has_persistent_identity() {
            return Err(WorkspaceError::PersistentIdentityUnavailable.into());
        }
        let epoch: i64 = self
            .connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        let environment = request_digest(
            "patch-recovery-grant.v1",
            &(operation, epoch, grant.environment()),
        )
        .map_err(|_| PatchError::Invalid)?;
        let result = PatchRecoveryGrant {
            grant,
            operation: operation.into(),
            environment,
            epoch,
        };
        self.unresolved_patch(&result, &result.grant)?;
        self.connection.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_consent' FROM operations WHERE operation_id=?",[operation]).map_err(JournalError::from)?;
        Ok(result)
    }
    fn unresolved_patch(
        &self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
    ) -> Result<(), PatchError> {
        let epoch: i64 = self
            .connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if epoch != grant.epoch {
            return Err(WorkspaceError::Revoked.into());
        }
        if grant.grant.is_revoked()
            || read.is_revoked()
            || grant.grant.owner() != read.owner()
            || grant.grant.policy() != read.policy()
            || grant.grant.resource_identity() != read.resource_identity()
        {
            return Err(WorkspaceError::Revoked.into());
        }
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id) WHERE o.operation_id=? AND o.owner=? AND o.policy=? AND o.state='unknown' AND o.tool_version='apply_patch.v1' AND p.resource_identity=? AND NOT EXISTS(SELECT 1 FROM patch_recovery_resolutions r WHERE r.operation_id=o.operation_id))",params![grant.operation,grant.grant.owner(),grant.grant.policy(),grant.grant.resource_identity()],|r|r.get(0)).map_err(JournalError::from)?;
        if !valid {
            return Err(PatchError::RecoveryRequired);
        }
        Ok(())
    }
    /// Request keys are immutable: changing a decision or observed version
    /// requires a new review, never replacing what was shown for approval.
    pub async fn prepare_patch_resolution(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        request_key: &str,
        choice: PatchRecoveryChoice,
    ) -> Result<PatchResolutionReview, PatchError> {
        if !valid_id(request_key) {
            return Err(PatchError::Invalid);
        }
        self.unresolved_patch(grant, read)?;
        let observed = self
            .inspect_patch_recovery(grant.grant.owner(), &grant.operation, read)
            .await?;
        let snapshot = observed.snapshot()?.clone();
        if !valid_choice(choice, &snapshot) {
            return Err(PatchError::VersionChanged);
        }
        let write = grant
            .grant
            .permit(
                grant.grant.owner(),
                grant.grant.environment(),
                grant.grant.policy(),
            )
            .await?;
        self.unresolved_patch(grant, read)?;
        let record = ResolutionRecord {
            owner: grant.grant.owner().into(),
            operation: grant.operation.clone(),
            environment: grant.environment.clone(),
            read_environment: read.environment().into(),
            resource: read.resource_identity().into(),
            policy: read.policy().into(),
            choice,
            snapshot: snapshot.clone(),
        };
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let digest = request_digest("patch-resolution-review.v1", &record)
            .map_err(|_| PatchError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM patch_recoveries WHERE operation_id=? AND state='dispatching')",[&grant.operation],|r|r.get(0)).map_err(JournalError::from)?;
        if busy {
            return Err(PatchError::RecoveryRequired);
        }
        let existing:Option<(String,String,String,i64)>=tx.query_row("SELECT recovery_id,body,state,host_epoch FROM patch_recoveries WHERE owner=? AND request_key=?",params![record.owner,request_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(JournalError::from)?;
        let epoch: i64 = tx
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        let id = if let Some((id, old, state, old_epoch)) = existing {
            if old != body || state != "intended" || old_epoch != epoch {
                return Err(JournalError::Conflict.into());
            }
            id
        } else {
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM patch_recoveries WHERE owner=?",
                    [&record.owner],
                    |r| r.get(0),
                )
                .map_err(JournalError::from)?;
            if count >= 1000 {
                return Err(PatchError::Limit);
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO patch_recoveries(recovery_id,operation_id,owner,request_key,host_epoch,environment,read_environment,body,digest,state) VALUES(?,?,?,?,?,?,?,?,?,'intended')",params![id,record.operation,record.owner,request_key,epoch,record.environment,record.read_environment,body,digest]).map_err(JournalError::from)?;
            tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_reviewed' FROM operations WHERE operation_id=?",[&record.operation]).map_err(JournalError::from)?;
            id
        };
        if write.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        observed.snapshot()?;
        tx.commit().map_err(JournalError::from)?;
        Ok(PatchResolutionReview {
            recovery_id: id,
            digest,
            choice,
            snapshot,
        })
    }
    fn resolution_record(
        &self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        recovery: &str,
        state: &str,
    ) -> Result<(ResolutionRecord, PatchResolutionReview), PatchError> {
        if !valid_id(recovery) {
            return Err(PatchError::Invalid);
        }
        self.unresolved_patch(grant, read)?;
        let row:Option<(String,String)>=self.connection.query_row("SELECT a.body,a.digest FROM patch_recoveries a JOIN runtime_host h ON h.singleton=1 WHERE a.recovery_id=? AND a.operation_id=? AND a.owner=? AND a.environment=? AND a.read_environment=? AND a.host_epoch=h.epoch AND a.state=?",params![recovery,grant.operation,grant.grant.owner(),grant.environment,read.environment(),state],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        let (body, digest) = row.ok_or(PatchError::RecoveryRequired)?;
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: ResolutionRecord =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if request_digest("patch-resolution-review.v1", &record).map_err(|_| PatchError::Invalid)?
            != digest
            || record.owner != grant.grant.owner()
            || record.operation != grant.operation
            || record.environment != grant.environment
            || record.read_environment != read.environment()
            || record.resource != read.resource_identity()
            || record.policy != read.policy()
            || record.snapshot.journal.plan.operation_id != grant.operation
            || !valid_choice(record.choice, &record.snapshot)
        {
            return Err(JournalError::Conflict.into());
        }
        let review = PatchResolutionReview {
            recovery_id: recovery.into(),
            digest,
            choice: record.choice,
            snapshot: record.snapshot.clone(),
        };
        Ok((record, review))
    }
    pub async fn approve_patch_resolution(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        recovery: &str,
        expected: &str,
        actor: &str,
        expires_at: i64,
    ) -> Result<(), PatchError> {
        if actor != grant.grant.owner() || expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        let (_, review) = self.resolution_record(grant, read, recovery, "intended")?;
        if review.digest != expected {
            return Err(JournalError::Conflict.into());
        }
        let observed = self
            .inspect_patch_recovery(actor, &grant.operation, read)
            .await?;
        if observed.snapshot()? != &review.snapshot {
            return Err(PatchError::VersionChanged);
        }
        self.resolution_record(grant, read, recovery, "intended")?;
        let write = grant
            .grant
            .permit(actor, grant.grant.environment(), grant.grant.policy())
            .await?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        tx.execute("INSERT INTO patch_recovery_approvals(recovery_id,digest,actor,expires_at) VALUES(?,?,?,?) ON CONFLICT(recovery_id) DO NOTHING",params![recovery,expected,actor,expires_at]).map_err(JournalError::from)?;
        let same:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM patch_recovery_approvals WHERE recovery_id=? AND digest=? AND actor=? AND expires_at=? AND expires_at>?)",params![recovery,expected,actor,expires_at,now()?],|r|r.get(0)).map_err(JournalError::from)?;
        if !same || write.inner.is_revoked() {
            return Err(JournalError::ApprovalRequired.into());
        }
        observed.snapshot()?;
        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_approved' FROM operations WHERE operation_id=?",[&grant.operation]).map_err(JournalError::from)?;
        tx.commit().map_err(JournalError::from)?;
        Ok(())
    }
    fn resolution_approval(
        &self,
        review: &PatchResolutionReview,
        owner: &str,
    ) -> Result<(), PatchError> {
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM patch_recovery_approvals WHERE recovery_id=? AND digest=? AND actor=? AND expires_at>?)",params![review.recovery_id,review.digest,owner,now()?],|r|r.get(0)).map_err(JournalError::from)?;
        if !valid {
            return Err(JournalError::ApprovalRequired.into());
        }
        Ok(())
    }
    pub async fn dispatch_patch_resolution(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        recovery: &str,
    ) -> Result<PatchResolutionDispatch, PatchError> {
        let (record, review) = self.resolution_record(grant, read, recovery, "intended")?;
        self.resolution_approval(&review, grant.grant.owner())?;
        let observed = self
            .inspect_patch_recovery(grant.grant.owner(), &grant.operation, read)
            .await?;
        if observed.snapshot()? != &record.snapshot {
            return Err(PatchError::VersionChanged);
        }
        let write = grant
            .grant
            .permit(
                grant.grant.owner(),
                grant.grant.environment(),
                grant.grant.policy(),
            )
            .await?;
        self.resolution_record(grant, read, recovery, "intended")?;
        self.resolution_approval(&review, grant.grant.owner())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM patch_recoveries WHERE operation_id=? AND state='dispatching')",[&grant.operation],|r|r.get(0)).map_err(JournalError::from)?;
        if busy {
            return Err(PatchError::RecoveryRequired);
        }
        if tx.execute("UPDATE patch_recoveries SET state='dispatching' WHERE recovery_id=? AND state='intended'",[recovery]).map_err(JournalError::from)?!=1 { return Err(JournalError::AlreadyDispatched.into()); }
        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_dispatching' FROM operations WHERE operation_id=?",[&grant.operation]).map_err(JournalError::from)?;
        if write.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        observed.snapshot()?;
        tx.commit().map_err(JournalError::from)?;
        Ok(PatchResolutionDispatch {
            record,
            review,
            read: observed,
            write,
        })
    }
}
