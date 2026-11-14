//! Synchronous filesystem lane, like the original trusted-host publisher.
//! All moves are NOREPLACE; no retained object is deleted or quota released.
use super::*;
use crate::workspace::patch::authority;
use crate::workspace::{directory_identity, open_directory};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, OpenOptions};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchResolutionResult {
    pub recovery_id: String,
    pub operation_id: String,
    pub choice: PatchRecoveryChoice,
    pub path: String,
    pub target: RecoveryFileObservation,
    pub retained_staged: RecoveryFileObservation,
    pub retained_original: RecoveryFileObservation,
}
fn io_error(error: std::io::Error) -> PatchError {
    WorkspaceError::Io(error).into()
}
fn parent_sync(parent: &Dir) -> Result<(), PatchError> {
    parent
        .try_clone()
        .map_err(io_error)?
        .into_std_file()
        .sync_all()
        .map_err(io_error)
}
fn named(parent: &Dir, name: &str, file: &File) -> Result<(), PatchError> {
    let a = authority::identity(parent, &parent.symlink_metadata(name).map_err(io_error)?)?;
    let b = authority::identity(parent, &file.metadata().map_err(io_error)?)?;
    if a != b {
        return Err(PatchError::VersionChanged);
    }
    Ok(())
}
fn unchanged_content(a: &RecoveryFileObservation, b: &RecoveryFileObservation) -> bool {
    match (a, b) {
        (
            RecoveryFileObservation::Regular {
                bytes: a_bytes,
                sha256: a_sha,
                security_sha256: a_security,
                reviewed_original_inode: a_inode,
                matches_original_content: a_old,
                matches_result_content: a_new,
                ..
            },
            RecoveryFileObservation::Regular {
                bytes: b_bytes,
                sha256: b_sha,
                security_sha256: b_security,
                reviewed_original_inode: b_inode,
                matches_original_content: b_old,
                matches_result_content: b_new,
                ..
            },
        ) => {
            (a_bytes, a_sha, a_security, a_inode, a_old, a_new)
                == (b_bytes, b_sha, b_security, b_inode, b_old, b_new)
        }
        _ => false,
    }
}
impl Journal {
    fn check_resolution_dispatch(
        &self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        token: &PatchResolutionDispatch,
    ) -> Result<(), PatchError> {
        let (_, review) =
            self.resolution_record(grant, read, &token.review.recovery_id, "dispatching")?;
        if review != token.review || !token.has_live_directory_access() {
            return Err(WorkspaceError::Revoked.into());
        }
        self.resolution_approval(&review, grant.grant.owner())
    }
    /// Consumes a separately approved recovery decision, never the original
    /// model operation. Recovery errors remain unknown and require a new review.
    pub fn publish_patch_resolution(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        token: PatchResolutionDispatch,
    ) -> Result<PatchResolutionResult, PatchError> {
        self.resolve_patch_with_hook(grant, read, token, |_| Ok(()))
    }
    pub(super) fn resolve_patch_with_hook(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        token: PatchResolutionDispatch,
        hook: impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<PatchResolutionResult, PatchError> {
        self.resolve_patch_checked(grant, read, token, Ok(()), hook)
    }
    pub(super) fn resolve_patch_checked(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        token: PatchResolutionDispatch,
        precheck: Result<(), PatchError>,
        mut hook: impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<PatchResolutionResult, PatchError> {
        // A selected scope can be revoked after durable dispatch and before
        // this call. Run the same unknown-state cleanup even for that early
        // refusal; never leave a live dispatch marker merely by dropping it.
        let result =
            precheck.and_then(|()| self.resolve_patch_inner(grant, read, &token, &mut hook));
        if result.is_err() {
            token
                .write
                .inner
                .revoked
                .store(true, std::sync::atomic::Ordering::Release);
            if let Ok(tx) = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
            {
                let saved = (|| -> Result<(), rusqlite::Error> {
                    let n=tx.execute("UPDATE patch_recoveries SET state='unknown' WHERE recovery_id=? AND state='dispatching'",[&token.review.recovery_id])?;
                    if n == 1 {
                        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_unknown' FROM operations WHERE operation_id=?",[&token.record.operation])?;
                    }
                    Ok(())
                })();
                if saved.is_ok() {
                    let _ = tx.commit();
                }
            }
        }
        result
    }
    fn resolve_patch_inner(
        &mut self,
        grant: &PatchRecoveryGrant,
        read: &WorkspaceGrant,
        token: &PatchResolutionDispatch,
        hook: &mut impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<PatchResolutionResult, PatchError> {
        self.check_resolution_dispatch(grant, read, token)?;
        let plan = &token.record.snapshot.journal.plan;
        let source_body: String = self
            .connection
            .query_row(
                "SELECT body FROM editing_previews WHERE operation_id=?",
                [&token.record.operation],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if source_body.len() > 512 * 1024 {
            return Err(PatchError::Limit);
        }
        let reviewed: Record =
            serde_json::from_str(&source_body).map_err(|_| PatchError::Invalid)?;
        if request_digest("patch-review.v1", &reviewed).map_err(|_| PatchError::Invalid)?
            != plan.preview_digest
        {
            return Err(JournalError::Conflict.into());
        }
        let root = &token.write.inner.directory;
        let names = components(&plan.path, false)?;
        let (provided_target, parents) = names.split_last().ok_or(PatchError::Invalid)?;
        // Legacy plans remain readable, but a missing native target must not
        // be guessed when restoring a namespace. Keeping a current file has
        // no rename effect and can still be explicitly acknowledged.
        let native_target = if plan.target_entry_name.is_some() {
            plan.native_target()?
        } else if token.record.choice == PatchRecoveryChoice::KeepCurrentTarget {
            *provided_target
        } else {
            return Err(PatchError::RecoveryRequired);
        };
        let target = &native_target;
        let parent = open_directory(root, parents)?;
        if !reviewed.source_identity.same_parent(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let observe=|| ->Result<(RecoveryFileObservation,RecoveryFileObservation,RecoveryFileObservation),PatchError>{
            Ok((inspection::observe(&parent,target,&reviewed.source_identity,plan,token.record.snapshot.journal.capture_security.as_ref())?,inspection::observe(&parent,&plan.staged_name,&reviewed.source_identity,plan,token.record.snapshot.journal.capture_security.as_ref())?,inspection::observe(&parent,&plan.backup_name,&reviewed.source_identity,plan,token.record.snapshot.journal.capture_security.as_ref())?))
        };
        let (before_target, before_staged, before_original) = observe()?;
        if (&before_target, &before_staged, &before_original)
            != (
                &token.record.snapshot.target,
                &token.record.snapshot.staged,
                &token.record.snapshot.original,
            )
        {
            return Err(PatchError::VersionChanged);
        }
        let selected = match token.record.choice {
            PatchRecoveryChoice::KeepCurrentTarget => *target,
            PatchRecoveryChoice::RestoreOriginalToMissingTarget => &plan.backup_name,
            PatchRecoveryChoice::PublishStagedToMissingTarget => &plan.staged_name,
        };
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let selected_fd = parent.open_with(selected, &options).map_err(io_error)?;
        named(&parent, selected, &selected_fd)?;
        let selected_identity =
            authority::identity(&parent, &selected_fd.metadata().map_err(io_error)?)?;
        hook(1)?;
        self.check_resolution_dispatch(grant, read, token)?;
        let current = observe()?;
        if current
            != (
                before_target.clone(),
                before_staged.clone(),
                before_original.clone(),
            )
        {
            return Err(PatchError::VersionChanged);
        }
        if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        named(&parent, selected, &selected_fd)?;
        if token.record.choice != PatchRecoveryChoice::KeepCurrentTarget {
            rustix::fs::renameat_with(
                &parent,
                selected,
                &parent,
                *target,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|e| io_error(e.into()))?;
        }
        hook(2)?;
        parent_sync(&parent)?;
        named(&parent, target, &selected_fd)?;
        if !selected_identity.matches_after_rename(&authority::identity(
            &parent,
            &selected_fd.metadata().map_err(io_error)?,
        )?) {
            return Err(PatchError::VersionChanged);
        }
        let (after_target, after_staged, after_original) = observe()?;
        let expected = match token.record.choice {
            PatchRecoveryChoice::KeepCurrentTarget => {
                after_target == before_target
                    && after_staged == before_staged
                    && after_original == before_original
            }
            PatchRecoveryChoice::RestoreOriginalToMissingTarget => {
                unchanged_content(&after_target, &before_original)
                    && matches!(after_original, RecoveryFileObservation::Missing)
                    && after_staged == before_staged
            }
            PatchRecoveryChoice::PublishStagedToMissingTarget => {
                unchanged_content(&after_target, &before_staged)
                    && matches!(after_staged, RecoveryFileObservation::Missing)
                    && after_original == before_original
            }
        };
        if !expected
            || directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)?
        {
            return Err(PatchError::VersionChanged);
        }
        hook(3)?;
        self.check_resolution_dispatch(grant, read, token)?;
        // Recheck even after the final fault hook: a hook models an external
        // race, not a reason to acknowledge stale filesystem evidence.
        if observe()?
            != (
                after_target.clone(),
                after_staged.clone(),
                after_original.clone(),
            )
        {
            return Err(PatchError::VersionChanged);
        }
        let receipt = PatchResolutionResult {
            recovery_id: token.review.recovery_id.clone(),
            operation_id: token.record.operation.clone(),
            choice: token.record.choice,
            path: plan.path.clone(),
            target: after_target,
            retained_staged: after_staged,
            retained_original: after_original,
        };
        let body = serde_json::to_string(&receipt).map_err(|_| PatchError::Invalid)?;
        let digest = request_digest("patch-resolution-result.v1", &receipt)
            .map_err(|_| PatchError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        if tx.execute("UPDATE patch_recoveries SET state='succeeded' WHERE recovery_id=? AND state='dispatching' AND EXISTS(SELECT 1 FROM patch_recovery_approvals a WHERE a.recovery_id=patch_recoveries.recovery_id AND a.expires_at>?)",params![receipt.recovery_id,now()?]).map_err(JournalError::from)?!=1 {return Err(JournalError::Transition.into());}
        tx.execute(
            "INSERT INTO patch_recovery_results(recovery_id,body,digest) VALUES(?,?,?)",
            params![receipt.recovery_id, body, digest],
        )
        .map_err(JournalError::from)?;
        tx.execute(
            "INSERT INTO patch_recovery_resolutions(operation_id,recovery_id) VALUES(?,?)",
            params![receipt.operation_id, receipt.recovery_id],
        )
        .map_err(JournalError::from)?;
        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'patch.recovery_resolved' FROM operations WHERE operation_id=?",[&receipt.operation_id]).map_err(JournalError::from)?;
        if !token.has_live_directory_access() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(receipt)
    }
    /// Historical resolution receipt; this is not a current filesystem check,
    /// nor a claim that the original model operation succeeded or was replayed.
    pub fn patch_resolution_result(
        &self,
        owner: &str,
        operation: &str,
    ) -> Result<Option<PatchResolutionResult>, PatchError> {
        if !valid_id(owner) || !valid_id(operation) {
            return Err(PatchError::Invalid);
        }
        let row:Option<(String,String,String)>=self.connection.query_row("SELECT r.body,r.digest,a.recovery_id FROM patch_recovery_resolutions s JOIN patch_recovery_results r USING(recovery_id) JOIN patch_recoveries a USING(recovery_id) WHERE s.operation_id=? AND a.operation_id=s.operation_id AND a.owner=? AND a.state='succeeded'",params![operation,owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(JournalError::from)?;
        let Some((body, digest, id)) = row else {
            return Ok(None);
        };
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let result: PatchResolutionResult =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if result.recovery_id != id
            || result.operation_id != operation
            || request_digest("patch-resolution-result.v1", &result)
                .map_err(|_| PatchError::Invalid)?
                != digest
        {
            return Err(JournalError::Conflict.into());
        }
        Ok(Some(result))
    }
}
