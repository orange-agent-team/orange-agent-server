//! Explicit bounded retention in place. Only accounting changes: no file is
//! moved, copied, deleted, or restored; old writable descriptors remain valid.
use super::*;
use inspection::{PatchRecoveryReceipt, RecoveryFileObservation};
const ARCHIVE_LIMIT: i64 = 256 * 1024 * 1024;
const ARCHIVE_RECORDS: i64 = 1000;
const FILE_RESERVATION: i64 = MAX_TEXT as i64 + 384 * 1024;
const REVIEW_LIFETIME: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchRetentionReview {
    pub review_id: String,
    pub digest: String,
    pub operation_id: String,
    pub path: String,
    pub staged: RecoveryFileObservation,
    pub original: RecoveryFileObservation,
    pub archive_reserved_bytes: String,
    pub expires_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchRetentionArchive {
    pub operation_id: String,
    pub review_id: String,
    pub review_digest: String,
    pub owner: String,
    pub resource_identity: String,
    pub plan: PublicationPlan,
    pub staged: RecoveryFileObservation,
    pub original: RecoveryFileObservation,
    pub archive_reserved_bytes: String,
    pub archived_at: i64,
    pub disposition: RetentionDisposition,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDisposition {
    RetainedInPlace,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRetentionUsage {
    pub resource_active_records: i64,
    pub resource_active_reserved_bytes: String,
    pub owner_archive_records: i64,
    pub owner_archive_reserved_bytes: String,
    pub owner_archive_limit_bytes: String,
    pub owner_archive_record_limit: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchArchiveView {
    pub archive: PatchRetentionArchive,
    pub current_staged: RecoveryFileObservation,
    pub current_original: RecoveryFileObservation,
    pub changed_since_archival: bool,
}
pub struct PatchArchiveInspection {
    archive: PatchRetentionArchive,
    live: PatchRecoveryReceipt,
}
impl PatchArchiveInspection {
    pub fn view(&self) -> Result<PatchArchiveView, PatchError> {
        let snapshot = self.live.snapshot()?;
        Ok(PatchArchiveView {
            archive: self.archive.clone(),
            current_staged: snapshot.staged.clone(),
            current_original: snapshot.original.clone(),
            changed_since_archival: self.archive.staged != snapshot.staged
                || self.archive.original != snapshot.original,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionRecord {
    owner: String,
    resource: String,
    read_environment: String,
    policy: String,
    plan: PublicationPlan,
    terminal_digest: String,
    staged: RecoveryFileObservation,
    original: RecoveryFileObservation,
    charged_bytes: i64,
    epoch: i64,
    expires_at: i64,
}
fn reservation(observation: &RecoveryFileObservation) -> Result<i64, PatchError> {
    match observation {
        RecoveryFileObservation::Missing => Ok(0),
        RecoveryFileObservation::Regular { bytes, .. }
            if bytes
                .parse::<usize>()
                .is_ok_and(|n| n <= MAX_TEXT && n.to_string() == *bytes) =>
        {
            Ok(FILE_RESERVATION)
        }
        _ => Err(PatchError::RecoveryRequired),
    }
}
impl RetentionRecord {
    fn digest(&self) -> Result<String, PatchError> {
        request_digest("patch-retention-review.v1", self).map_err(|_| PatchError::Invalid)
    }
    fn review(&self, id: String) -> Result<PatchRetentionReview, PatchError> {
        Ok(PatchRetentionReview {
            review_id: id,
            digest: self.digest()?,
            operation_id: self.plan.operation_id.clone(),
            path: self.plan.path.clone(),
            staged: self.staged.clone(),
            original: self.original.clone(),
            archive_reserved_bytes: self.charged_bytes.to_string(),
            expires_at: self.expires_at,
        })
    }
}
impl Journal {
    fn retention_terminal(&self, owner: &str, operation: &str) -> Result<String, PatchError> {
        let view = self
            .patch_recovery_view(owner, operation)?
            .ok_or(WorkspaceError::Forbidden)?;
        if view.execution_state == "succeeded" {
            let receipt = view.committed_receipt.ok_or(PatchError::RecoveryRequired)?;
            return request_digest("retention-confirmed-publication.v1", &receipt)
                .map_err(|_| PatchError::Invalid);
        }
        if view.execution_state == "unknown"
            && let Some(receipt) = self.patch_resolution_result(owner, operation)?
        {
            return request_digest("retention-confirmed-resolution.v1", &receipt)
                .map_err(|_| PatchError::Invalid);
        }
        Err(PatchError::RecoveryRequired)
    }
    fn retention_scope(&self, owner: &str, read: &WorkspaceGrant) -> Result<i64, PatchError> {
        if !valid_id(owner) || read.owner() != owner || read.is_revoked() {
            return Err(WorkspaceError::Forbidden.into());
        }
        self.connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| JournalError::from(e).into())
    }
    /// Owner confirmation is separate from tool approval. A read grant allows
    /// preview only; it never archives files or grants filesystem write access.
    pub async fn prepare_patch_retention(
        &mut self,
        owner: &str,
        operation: &str,
        read: &WorkspaceGrant,
        request_key: &str,
    ) -> Result<PatchRetentionReview, PatchError> {
        if !valid_id(request_key) {
            return Err(PatchError::Invalid);
        }
        let epoch = self.retention_scope(owner, read)?;
        let terminal_digest = self.retention_terminal(owner, operation)?;
        if self.patch_retention_archive(owner, operation)?.is_some() {
            return Err(JournalError::Conflict.into());
        }
        let prior:Option<(String,String)>=self.connection.query_row("SELECT review_id,body FROM patch_retention_reviews WHERE owner=? AND request_key=?",params![owner,request_key],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        let expires_at = if let Some((_, body)) = &prior {
            if body.len() > 32 * 1024 {
                return Err(PatchError::Limit);
            }
            let old: RetentionRecord =
                serde_json::from_str(body).map_err(|_| PatchError::Invalid)?;
            old.expires_at
        } else {
            now()?
                .checked_add(REVIEW_LIFETIME)
                .ok_or(PatchError::Invalid)?
        };
        if expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        let observed = self.inspect_patch_recovery(owner, operation, read).await?;
        let snapshot = observed.snapshot()?;
        let charged_bytes = reservation(&snapshot.staged)? + reservation(&snapshot.original)?;
        let record = RetentionRecord {
            owner: owner.into(),
            resource: read.resource_identity().into(),
            read_environment: read.environment().into(),
            policy: read.policy().into(),
            plan: snapshot.journal.plan.clone(),
            terminal_digest,
            staged: snapshot.staged.clone(),
            original: snapshot.original.clone(),
            charged_bytes,
            epoch,
            expires_at,
        };
        if self.retention_scope(owner, read)? != epoch
            || self.retention_terminal(owner, operation)? != record.terminal_digest
        {
            return Err(PatchError::RecoveryRequired);
        }
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let digest = record.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let id = if let Some((id, old)) = prior {
            if old != body {
                return Err(JournalError::Conflict.into());
            }
            id
        } else {
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM patch_retention_reviews WHERE owner=?",
                    [owner],
                    |r| r.get(0),
                )
                .map_err(JournalError::from)?;
            if count >= 1000 {
                return Err(PatchError::Limit);
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO patch_retention_reviews(review_id,operation_id,owner,request_key,host_epoch,expires_at,body,digest) VALUES(?,?,?,?,?,?,?,?)",params![id,operation,owner,request_key,epoch,expires_at,body,digest]).map_err(JournalError::from)?;
            tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.retention_reviewed' FROM operations WHERE operation_id=?",[operation]).map_err(JournalError::from)?;
            id
        };
        observed.snapshot()?;
        tx.commit().map_err(JournalError::from)?;
        record.review(id)
    }
    fn retention_record(
        &self,
        owner: &str,
        read: &WorkspaceGrant,
        review: &str,
        expected: &str,
    ) -> Result<RetentionRecord, PatchError> {
        if !valid_id(review) {
            return Err(PatchError::Invalid);
        }
        let epoch = self.retention_scope(owner, read)?;
        let row:Option<(String,String,i64,i64,String)>=self.connection.query_row("SELECT body,digest,host_epoch,expires_at,operation_id FROM patch_retention_reviews WHERE owner=? AND review_id=?",params![owner,review],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(JournalError::from)?;
        let (body, digest, recorded_epoch, expiry, operation) =
            row.ok_or(WorkspaceError::Forbidden)?;
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: RetentionRecord =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if expected != digest
            || record.digest()? != digest
            || record.owner != owner
            || record.plan.operation_id != operation
            || record.epoch != recorded_epoch
            || record.epoch != epoch
            || record.expires_at != expiry
            || record.resource != read.resource_identity()
            || record.read_environment != read.environment()
            || record.policy != read.policy()
            || record.charged_bytes != reservation(&record.staged)? + reservation(&record.original)?
        {
            return Err(JournalError::Conflict.into());
        }
        Ok(record)
    }
    /// Commit one explicit archival decision. No filesystem effects occur.
    /// An ambiguous database response is queried/retried by the same review ID;
    /// the unique operation key prevents duplicate archive budget charges.
    pub async fn approve_patch_retention(
        &mut self,
        owner: &str,
        read: &WorkspaceGrant,
        review: &str,
        expected: &str,
        actor: &str,
    ) -> Result<PatchRetentionArchive, PatchError> {
        if actor != owner {
            return Err(JournalError::ApprovalRequired.into());
        }
        let record = self.retention_record(owner, read, review, expected)?;
        if let Some(archive) = self.patch_retention_archive(owner, &record.plan.operation_id)? {
            if archive.review_id != review || archive.review_digest != expected {
                return Err(JournalError::Conflict.into());
            }
            return Ok(archive);
        }
        if record.expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        if self.retention_terminal(owner, &record.plan.operation_id)? != record.terminal_digest {
            return Err(PatchError::RecoveryRequired);
        }
        let observed = self
            .inspect_patch_recovery(owner, &record.plan.operation_id, read)
            .await?;
        let current = observed.snapshot()?;
        // The target may legitimately change while a user reviews retention;
        // only the two retained objects are archived and no target is modified.
        if current.journal.plan != record.plan
            || current.staged != record.staged
            || current.original != record.original
        {
            return Err(PatchError::VersionChanged);
        }
        self.retention_record(owner, read, review, expected)?;
        if self.retention_terminal(owner, &record.plan.operation_id)? != record.terminal_digest {
            return Err(PatchError::RecoveryRequired);
        }
        let archived_at = now()?;
        if record.expires_at <= archived_at {
            return Err(JournalError::ApprovalRequired.into());
        }
        let archive = PatchRetentionArchive {
            operation_id: record.plan.operation_id.clone(),
            review_id: review.into(),
            review_digest: expected.into(),
            owner: owner.into(),
            resource_identity: record.resource.clone(),
            plan: record.plan.clone(),
            staged: record.staged,
            original: record.original,
            archive_reserved_bytes: record.charged_bytes.to_string(),
            archived_at,
            disposition: RetentionDisposition::RetainedInPlace,
        };
        let body = serde_json::to_string(&archive).map_err(|_| PatchError::Invalid)?;
        let digest = request_digest("patch-retention-archive.v1", &archive)
            .map_err(|_| PatchError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(charged_bytes),0) FROM patch_retention_archives WHERE owner=?",[owner],|r|Ok((r.get(0)?,r.get(1)?))).map_err(JournalError::from)?;
        if count >= ARCHIVE_RECORDS
            || bytes
                .checked_add(record.charged_bytes)
                .is_none_or(|n| n > ARCHIVE_LIMIT)
        {
            return Err(PatchError::Limit);
        }
        tx.execute("INSERT INTO patch_retention_archives(operation_id,review_id,owner,resource_identity,charged_bytes,body,digest) VALUES(?,?,?,?,?,?,?)",params![archive.operation_id,review,owner,archive.resource_identity,record.charged_bytes,body,digest]).map_err(JournalError::from)?;
        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.retention_archived_in_place' FROM operations WHERE operation_id=?",[&archive.operation_id]).map_err(JournalError::from)?;
        observed.snapshot()?;
        tx.commit().map_err(JournalError::from)?;
        Ok(archive)
    }
    pub fn patch_retention_archive(
        &self,
        owner: &str,
        operation: &str,
    ) -> Result<Option<PatchRetentionArchive>, PatchError> {
        if !valid_id(owner) || !valid_id(operation) {
            return Err(PatchError::Invalid);
        }
        let row:Option<(String,String,String,i64,String)>=self.connection.query_row("SELECT body,digest,resource_identity,charged_bytes,review_id FROM patch_retention_archives WHERE owner=? AND operation_id=?",params![owner,operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(JournalError::from)?;
        let Some((body, digest, resource, bytes, review)) = row else {
            return Ok(None);
        };
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: PatchRetentionArchive =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if record.owner != owner
            || record.operation_id != operation
            || record.plan.operation_id != operation
            || record.resource_identity != resource
            || record.archive_reserved_bytes != bytes.to_string()
            || record.review_id != review
            || reservation(&record.staged)? + reservation(&record.original)? != bytes
            || request_digest("patch-retention-archive.v1", &record)
                .map_err(|_| PatchError::Invalid)?
                != digest
        {
            return Err(JournalError::Conflict.into());
        }
        Ok(Some(record))
    }
    pub async fn inspect_patch_archive(
        &self,
        owner: &str,
        operation: &str,
        read: &WorkspaceGrant,
    ) -> Result<PatchArchiveInspection, PatchError> {
        self.retention_scope(owner, read)?;
        let archive = self
            .patch_retention_archive(owner, operation)?
            .ok_or(WorkspaceError::Forbidden)?;
        if archive.resource_identity != read.resource_identity() {
            return Err(WorkspaceError::Forbidden.into());
        }
        let live = self.inspect_patch_recovery(owner, operation, read).await?;
        Ok(PatchArchiveInspection { archive, live })
    }
    /// Admission reservations, not physical block usage or a filesystem quota.
    pub fn patch_retention_usage(
        &self,
        owner: &str,
        read: &WorkspaceGrant,
    ) -> Result<PatchRetentionUsage, PatchError> {
        self.retention_scope(owner, read)?;
        let (active,bytes):(i64,i64)=self.connection.query_row("SELECT count(*),coalesce(sum(p.reserved_bytes),0) FROM editing_publications p WHERE p.resource_identity=? AND NOT EXISTS(SELECT 1 FROM patch_retention_archives a WHERE a.operation_id=p.operation_id)",[read.resource_identity()],|r|Ok((r.get(0)?,r.get(1)?))).map_err(JournalError::from)?;
        let (archives,archive_bytes):(i64,i64)=self.connection.query_row("SELECT count(*),coalesce(sum(charged_bytes),0) FROM patch_retention_archives WHERE owner=?",[owner],|r|Ok((r.get(0)?,r.get(1)?))).map_err(JournalError::from)?;
        Ok(PatchRetentionUsage {
            resource_active_records: active,
            resource_active_reserved_bytes: bytes.to_string(),
            owner_archive_records: archives,
            owner_archive_reserved_bytes: archive_bytes.to_string(),
            owner_archive_limit_bytes: ARCHIVE_LIMIT.to_string(),
            owner_archive_record_limit: ARCHIVE_RECORDS,
        })
    }
}
#[cfg(test)]
mod tests;
