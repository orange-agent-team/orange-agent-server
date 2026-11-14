//! Durable exact review for a trusted adapter. Apple publication lives behind
//! a separate consumed dispatch token. Normal Runs additionally require their
//! immutable edit capability and engine contract/approval bindings.
use super::authority::{AuthorizedPatchPreview, EditFileGrant, SourceIdentity};
use super::*;
use crate::{Journal, JournalError, Operation, workspace::WorkspaceGrant};
use orange_protocol::{model::ResponseCandidate, valid_id};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::time::{SystemTime, UNIX_EPOCH};
pub mod dispatch;
#[cfg(target_vendor = "apple")]
pub mod publication;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchReview {
    pub operation_id: String,
    pub preview_digest: String,
    pub preview: PatchPreview,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) operation_id: String,
    pub(super) owner: String,
    pub(super) run: String,
    pub(super) invocation: String,
    pub(super) call_id: String,
    pub(super) consent_id: String,
    pub(super) consent_version: String,
    pub(super) environment: String,
    pub(super) read_environment: String,
    pub(super) policy: String,
    pub(super) args: PatchArgs,
    pub(super) preview: PatchPreview,
    pub(super) source_binding: String,
    pub(super) source_identity: SourceIdentity,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    tool: String,
    arguments: PatchArgs,
}
/// Prepared execution evidence only. No filesystem write has occurred, and the
/// eventual publisher must recheck expiry/state and commit dispatch first.
pub struct ApprovedPatch {
    pub(super) actual: AuthorizedPatchPreview,
    pub(super) review: PatchReview,
}
impl ApprovedPatch {
    pub fn review(&self) -> &PatchReview {
        &self.review
    }
    pub fn content(&self) -> &[u8] {
        self.actual.content()
    }
    /// Directory leases only; this does not replace a fresh approval/Run/expiry
    /// check and durable dispatch barrier in the eventual publisher.
    pub fn has_live_directory_access(&self) -> bool {
        self.actual.is_authorized()
    }
}
fn now() -> Result<i64, PatchError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PatchError::Invalid)?
            .as_secs(),
    )
    .map_err(|_| PatchError::Invalid)
}
impl Journal {
    fn patch_operation(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
        args: &PatchArgs,
    ) -> Result<(), PatchError> {
        args.validate()?;
        self.validate_edit_consent(edit)?;
        if op.tool_version != "apply_patch.v1"
            || !op.requires_approval
            || op.argument_digest != args.digest()?
            || op.owner != edit.owner()
            || op.policy_version != edit.policy()
            || op.environment != edit.environment()
            || read.owner() != edit.owner()
            || read.policy() != edit.policy()
            || read.resource_identity() != edit.resource_identity()
            || read.is_revoked()
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        let normal: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?)",
                [op.run],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if normal {
            #[cfg(target_vendor = "apple")]
            self.check_runtime_edit_intent(op, edit, false)
                .map_err(|_| PatchError::RecoveryRequired)?;
            #[cfg(not(target_vendor = "apple"))]
            return Err(PatchError::RecoveryRequired);
        }
        Ok(())
    }
    fn patch_response(
        &self,
        op: &Operation<'_>,
        args: &PatchArgs,
        call_id: &str,
    ) -> Result<(), PatchError> {
        if !valid_id(call_id) {
            return Err(PatchError::Invalid);
        }
        let row:Option<(String,String)>=self.connection.query_row("SELECT p.body,p.digest FROM responses p JOIN runs r USING(run_id) WHERE p.invocation=? AND p.run_id=? AND r.owner=? AND r.policy=? AND r.active=1 AND p.attempt=1 AND p.body IS NOT NULL",params![op.invocation,op.run,op.owner,op.policy_version],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        let (body, expected) = row.ok_or(JournalError::ResponseRequired)?;
        if body.len() > 256 * 1024 {
            return Err(PatchError::Limit);
        }
        let response: ResponseCandidate =
            serde_json::from_str(&body).map_err(|_| JournalError::ResponseRequired)?;
        if request_digest("model-response-v1", &response).map_err(|_| PatchError::Invalid)?
            != expected
            || response.finish_reason != "tool_calls"
            || !valid_id(&response.response_id)
            || response.tool_calls.is_empty()
            || response.tool_calls.len() > 80
        {
            return Err(JournalError::ResponseRequired.into());
        }
        let mut ids = std::collections::BTreeSet::new();
        if response
            .tool_calls
            .iter()
            .any(|call| !valid_id(&call.id) || !ids.insert(&call.id))
        {
            return Err(JournalError::ResponseRequired.into());
        }
        let call = response
            .tool_calls
            .iter()
            .find(|call| call.id == call_id)
            .ok_or(JournalError::ResponseRequired)?;
        let original: PatchArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|_| JournalError::ResponseRequired)?;
        if call.name != "apply_patch"
            || &original != args
            || original.digest()? != op.argument_digest
        {
            return Err(JournalError::Conflict.into());
        }
        Ok(())
    }
    pub async fn prepare_patch_review(
        &mut self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
        args: PatchArgs,
        call_id: &str,
    ) -> Result<PatchReview, PatchError> {
        self.patch_operation(edit, read, op, &args)?;
        self.patch_response(op, &args, call_id)?;
        let actual = self.preview_file_edit(edit, read, args.clone()).await?;
        let record = Record {
            operation_id: op.id.into(),
            owner: op.owner.into(),
            run: op.run.into(),
            invocation: op.invocation.into(),
            call_id: call_id.into(),
            consent_id: edit.consent_id().into(),
            consent_version: edit.consent_version().ok_or(WorkspaceError::Forbidden)?,
            environment: edit.environment().into(),
            read_environment: read.environment().into(),
            policy: op.policy_version.into(),
            args: args.clone(),
            preview: actual.preview().clone(),
            source_binding: actual.source_binding_digest().into(),
            source_identity: actual.identity.clone(),
        };
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        let digest = request_digest("patch-review.v1", &record).map_err(|_| PatchError::Invalid)?;
        let request = serde_json::to_string(&Request {
            tool: "apply_patch".into(),
            arguments: args,
        })
        .map_err(|_| PatchError::Invalid)?;
        self.prepare(op)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let row: (Option<String>, Option<String>, String) = tx
            .query_row(
                "SELECT request_json,call_id,state FROM operations WHERE operation_id=?",
                [op.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(JournalError::from)?;
        match row {
            (None, None, state) if state == "intended" => {
                tx.execute(
                    "UPDATE operations SET request_json=?,call_id=? WHERE operation_id=?",
                    params![request, call_id, op.id],
                )
                .map_err(JournalError::from)?;
            }
            (Some(old), Some(id), state)
                if old == request && id == call_id && state == "intended" => {}
            _ => return Err(JournalError::Conflict.into()),
        }
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT body,digest FROM editing_previews WHERE operation_id=?",
                [op.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(JournalError::from)?;
        if let Some((old, hash)) = prior {
            if old != body || hash != digest {
                return Err(JournalError::Conflict.into());
            }
        } else {
            tx.execute(
                "INSERT INTO editing_previews(operation_id,body,digest) VALUES(?,?,?)",
                params![op.id, body, digest],
            )
            .map_err(JournalError::from)?;
            tx.execute(
                "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.preview_bound')",
                params![op.run, op.id],
            )
            .map_err(JournalError::from)?;
        }
        if !actual.is_authorized() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(PatchReview {
            operation_id: op.id.into(),
            preview_digest: digest,
            preview: record.preview,
        })
    }
    fn patch_record(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
    ) -> Result<(Record, String), PatchError> {
        self.validate_edit_consent(edit)?;
        if read.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let row:Option<(String,String,String,String)>=self.connection.query_row("SELECT p.body,p.digest,o.request_json,o.call_id FROM editing_previews p JOIN operations o USING(operation_id) JOIN runs r USING(run_id) WHERE o.operation_id=? AND o.owner=? AND o.run_id=? AND o.invocation=? AND o.argument_digest=? AND o.environment=? AND o.policy=? AND o.tool_version='apply_patch.v1' AND o.requires_approval=1 AND r.owner=o.owner AND r.policy=o.policy AND r.active=1",params![op.id,op.owner,op.run,op.invocation,op.argument_digest,op.environment,op.policy_version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(JournalError::from)?;
        let (body, digest, request, call_id) = row.ok_or(JournalError::ResponseRequired)?;
        if body.len() > 512 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let request: Request = serde_json::from_str(&request).map_err(|_| PatchError::Invalid)?;
        if request_digest("patch-review.v1", &record).map_err(|_| PatchError::Invalid)? != digest
            || record.operation_id != op.id
            || record.owner != op.owner
            || record.run != op.run
            || record.invocation != op.invocation
            || record.policy != op.policy_version
            || record.call_id != call_id
            || record.consent_id != edit.consent_id()
            || Some(record.consent_version.clone()) != edit.consent_version()
            || record.environment != edit.environment()
            || record.read_environment != read.environment()
            || request.tool != "apply_patch"
            || request.arguments != record.args
            || record.preview.argument_digest != op.argument_digest
            || record.preview.original_sha256 != record.args.expected_sha256
        {
            return Err(JournalError::Conflict.into());
        }
        self.patch_operation(edit, read, op, &record.args)?;
        self.patch_response(op, &record.args, &record.call_id)?;
        Ok((record, digest))
    }
    async fn current_patch_review(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
    ) -> Result<(AuthorizedPatchPreview, PatchReview), PatchError> {
        let (record, digest) = self.patch_record(edit, read, op)?;
        let actual = self.preview_file_edit(edit, read, record.args).await?;
        if actual.preview() != &record.preview
            || actual.source_binding_digest() != record.source_binding
            || actual.identity != record.source_identity
        {
            return Err(PatchError::VersionChanged);
        }
        Ok((
            actual,
            PatchReview {
                operation_id: op.id.into(),
                preview_digest: digest,
                preview: record.preview,
            },
        ))
    }
    pub async fn patch_review(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
    ) -> Result<PatchReview, PatchError> {
        Ok(self.current_patch_review(edit, read, op).await?.1)
    }
    pub async fn approve_patch_review(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
        expected_preview: &str,
        actor: &str,
        expires_at: i64,
    ) -> Result<(), PatchError> {
        if expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        let (actual, review) = self.current_patch_review(edit, read, op).await?;
        if review.preview_digest != expected_preview {
            return Err(JournalError::Conflict.into());
        }
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
                .map_err(JournalError::from)?;
        self.approve(op, actor, expires_at)?;
        tx.execute("INSERT INTO editing_approvals(operation_id,preview_digest,consent_id,consent_version,read_environment) VALUES(?,?,?,?,?) ON CONFLICT(operation_id) DO NOTHING",params![op.id,review.preview_digest,edit.consent_id(),edit.consent_version().ok_or(WorkspaceError::Forbidden)?,read.environment()]).map_err(JournalError::from)?;
        self.require_patch_approval(edit, read, op, &review.preview_digest)?;
        if !actual.is_authorized() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(())
    }
    fn require_patch_approval(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
        digest: &str,
    ) -> Result<(), PatchError> {
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM editing_approvals p JOIN approvals a USING(operation_id) JOIN operations o USING(operation_id) WHERE p.operation_id=? AND p.preview_digest=? AND p.consent_id=? AND p.consent_version=? AND p.read_environment=? AND a.argument_digest=o.argument_digest AND a.environment=o.environment AND a.policy=o.policy AND a.expires_at>? AND o.state='intended')",params![op.id,digest,edit.consent_id(),edit.consent_version().ok_or(WorkspaceError::Forbidden)?,read.environment(),now()?],|r|r.get(0)).map_err(JournalError::from)?;
        if !valid {
            return Err(JournalError::ApprovalRequired.into());
        }
        Ok(())
    }
    pub async fn approved_patch(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        op: &Operation<'_>,
    ) -> Result<ApprovedPatch, PatchError> {
        let (actual, review) = self.current_patch_review(edit, read, op).await?;
        self.require_patch_approval(edit, read, op, &review.preview_digest)?;
        Ok(ApprovedPatch { actual, review })
    }
}
#[cfg(test)]
pub(super) mod tests;
