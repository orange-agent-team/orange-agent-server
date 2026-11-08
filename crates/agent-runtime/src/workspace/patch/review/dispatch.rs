//! Durable publication reservation. This module changes only the journal;
//! it does not write, rename, delete or automatically recover workspace files.
use super::*;

const MAX_PUBLICATIONS: i64 = 64;
const MAX_RESERVED_BYTES: i64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationPlan {
    pub operation_id: String,
    pub preview_digest: String,
    pub path: String,
    pub staged_name: String,
    pub backup_name: String,
    pub original_sha256: String,
    pub result_sha256: String,
    pub reserved_bytes: String,
}

/// Non-serializable ownership token. A plan read from SQLite is never a token.
/// Holding this value does not make an expired/revoked approval valid; a future
/// publisher must recheck current authorization and the actual source first.
pub struct PatchDispatch {
    pub(super) approved: ApprovedPatch,
    pub(super) plan: PublicationPlan,
}
impl PatchDispatch {
    pub fn plan(&self) -> &PublicationPlan {
        &self.plan
    }
    pub fn has_live_directory_access(&self) -> bool {
        self.approved.has_live_directory_access()
    }
}

impl Journal {
    /// Atomically reserve recovery names/capacity and consume exact approval
    /// at the durable dispatch barrier. No filesystem effect occurs here.
    /// Dropping the token does not undo dispatch: reopening makes it unknown.
    pub async fn begin_patch_publication(
        &mut self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
    ) -> Result<PatchDispatch, PatchError> {
        let approved = self.approved_patch(edit, read, op).await?;
        // Recheck after the filesystem await. The host owns the journal lock;
        // there are no further awaits between these checks and the commit.
        let (record, digest) = self.patch_record(edit, read, op)?;
        self.require_patch_approval(edit, read, op, &digest)?;
        if digest != approved.review.preview_digest || !approved.has_live_directory_access() {
            return Err(WorkspaceError::Revoked.into());
        }
        let original: i64 = record
            .preview
            .original_bytes
            .parse()
            .map_err(|_| PatchError::Invalid)?;
        let result: i64 = record
            .preview
            .result_bytes
            .parse()
            .map_err(|_| PatchError::Invalid)?;
        if !(0..=MAX_TEXT as i64).contains(&original) || !(0..=MAX_TEXT as i64).contains(&result) {
            return Err(PatchError::Limit);
        }
        // Bytes plus a bounded allowance for both files' metadata. This is
        // host admission accounting, not a filesystem block/quota guarantee.
        let reserved_bytes = original + result + 2 * 384 * 1024;
        let plan = PublicationPlan {
            operation_id: op.id.into(),
            preview_digest: digest,
            path: record.args.path,
            staged_name: format!(".orange-agent-tmp-patch-new-{}", uuid::Uuid::new_v4()),
            backup_name: format!(".orange-agent-tmp-patch-old-{}", uuid::Uuid::new_v4()),
            original_sha256: record.preview.original_sha256,
            result_sha256: record.preview.result_sha256,
            reserved_bytes: reserved_bytes.to_string(),
        };
        let body = serde_json::to_string(&plan).map_err(|_| PatchError::Invalid)?;
        let digest =
            request_digest("patch-publication-plan.v1", &plan).map_err(|_| PatchError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let (count, bytes): (i64, i64) = tx.query_row(
            "SELECT count(*),coalesce(sum(reserved_bytes),0) FROM editing_publications WHERE resource_identity=?",
            [edit.resource_identity()], |r| Ok((r.get(0)?,r.get(1)?)),
        ).map_err(JournalError::from)?;
        if count >= MAX_PUBLICATIONS
            || bytes
                .checked_add(reserved_bytes)
                .is_none_or(|n| n > MAX_RESERVED_BYTES)
        {
            return Err(PatchError::Limit);
        }
        // No re-use of generic Journal::claim: publication evidence and the
        // operation transition must be committed in the same SQLite transaction.
        let changed = tx.execute(
            "UPDATE operations SET state='dispatching' WHERE operation_id=? AND state='intended' AND EXISTS(SELECT 1 FROM runs r WHERE r.run_id=operations.run_id AND r.active=1 AND r.owner=operations.owner AND r.policy=operations.policy) AND EXISTS(SELECT 1 FROM approvals a WHERE a.operation_id=operations.operation_id AND a.expires_at>?)",
            params![op.id, now()?],
        ).map_err(JournalError::from)?;
        if changed != 1 {
            return Err(JournalError::AlreadyDispatched.into());
        }
        let inserted = tx.execute(
            "INSERT INTO editing_publications(operation_id,resource_identity,host_epoch,reserved_bytes,body,digest) SELECT ?,?,epoch,?,?,? FROM runtime_host WHERE singleton=1",
            params![op.id,edit.resource_identity(),reserved_bytes,body,digest],
        ).map_err(JournalError::from)?;
        if inserted != 1 {
            return Err(JournalError::Conflict.into());
        }
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.publication_reserved')",
            params![op.run, op.id],
        )
        .map_err(JournalError::from)?;
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.dispatching')",
            params![op.run, op.id],
        )
        .map_err(JournalError::from)?;
        if !approved.has_live_directory_access() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(PatchDispatch { approved, plan })
    }

    /// Recovery metadata for an authenticated owner, without opening the old
    /// workspace, returning file content, granting permission or retrying it.
    pub fn patch_publication_plan(
        &self,
        owner: &str,
        operation: &str,
    ) -> Result<Option<PublicationPlan>, PatchError> {
        if !valid_id(owner) || !valid_id(operation) {
            return Err(PatchError::Invalid);
        }
        let row: Option<(String,String)> = self.connection.query_row(
            "SELECT p.body,p.digest FROM editing_publications p JOIN operations o USING(operation_id) WHERE o.owner=? AND o.operation_id=?",
            params![owner,operation],|r| Ok((r.get(0)?,r.get(1)?)),
        ).optional().map_err(JournalError::from)?;
        let Some((body, digest)) = row else {
            return Ok(None);
        };
        if body.len() > 16 * 1024 {
            return Err(PatchError::Limit);
        }
        let plan: PublicationPlan = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if plan.operation_id != operation
            || request_digest("patch-publication-plan.v1", &plan)
                .map_err(|_| PatchError::Invalid)?
                != digest
        {
            return Err(JournalError::Conflict.into());
        }
        Ok(Some(plan))
    }
}
