//! Synchronous trusted-host publisher for the verified Apple backend. The
//! owning host must run this bounded-file work on its blocking execution lane.
//! The engine opts in with an immutable edit capability; names are never overwritten.
pub use super::dispatch::PublishedPatch;
use super::{dispatch::PatchDispatch, *};
use crate::workspace::{directory_identity, open_directory};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, OpenOptions, OpenOptionsExt};
use std::io::{Read, Seek, Write};

fn io_error(error: std::io::Error) -> PatchError {
    WorkspaceError::Io(error).into()
}
fn inspected(parent: &Dir, file: &File) -> Result<SourceIdentity, PatchError> {
    let mut identity =
        super::super::authority::identity(parent, &file.metadata().map_err(io_error)?)?;
    identity.bind_security(
        orange_platform_files::security_metadata(file)
            .map_err(io_error)?
            .digest(),
    );
    Ok(identity)
}
fn verify_bytes(file: &mut File, digest: &str) -> Result<(), PatchError> {
    file.rewind().map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_TEXT || hex::encode(sha2::Sha256::digest(&bytes)) != digest {
        return Err(PatchError::VersionChanged);
    }
    Ok(())
}
fn named_file(parent: &Dir, name: &str, file: &File) -> Result<(), PatchError> {
    let named = super::super::authority::identity(
        parent,
        &parent.symlink_metadata(name).map_err(io_error)?,
    )?;
    let opened = super::super::authority::identity(parent, &file.metadata().map_err(io_error)?)?;
    if named != opened {
        return Err(PatchError::VersionChanged);
    }
    Ok(())
}
fn sync_directory(parent: &Dir) -> Result<(), PatchError> {
    parent
        .try_clone()
        .map_err(io_error)?
        .into_std_file()
        .sync_all()
        .map_err(io_error)
}
fn move_without_replace(parent: &Dir, from: &str, to: &str) -> Result<(), PatchError> {
    rustix::fs::renameat_with(parent, from, parent, to, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(|e| io_error(e.into()))
}

impl Journal {
    fn live_patch_dispatch(
        &self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        token: &PatchDispatch,
    ) -> Result<Record, PatchError> {
        self.check_edit_permission(edit)?;

        let plan = &token.plan;
        if !token.has_live_directory_access()
            || read.is_revoked()
            || read.owner() != edit.owner()
            || read.policy() != edit.policy()
            || read.resource_identity() != edit.resource_identity()
            || token.approved.actual._read.inner.environment != read.environment()
            || token.approved.actual._edit.inner.resource_identity != edit.resource_identity()
            || self
                .patch_publication_plan(edit.owner(), &plan.operation_id)?
                .as_ref()
                != Some(plan)
        {
            return Err(WorkspaceError::Revoked.into());
        }
        let (body, digest): (String, String) = self
            .connection
            .query_row(
                "SELECT body,digest FROM editing_previews WHERE operation_id=?",
                [&plan.operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(JournalError::from)?;
        if body.len() > 512 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if digest != plan.preview_digest
            || request_digest("patch-review.v1", &record).map_err(|_| PatchError::Invalid)?
                != digest
            || record.consent_id != edit.consent_id()
            || Some(record.consent_version.clone()) != edit.consent_version()
            || record.owner != edit.owner()
            || record.environment != edit.environment()
            || record.read_environment != read.environment()
            || record.policy != edit.policy()
            || record.operation_id != plan.operation_id
            || record.source_identity != token.approved.actual.identity
            || record.preview != *token.approved.actual.preview()
            || plan.native_target()? != token.approved.actual.native_name
        {
            return Err(JournalError::Conflict.into());
        }
        let valid: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM editing_publications p JOIN runtime_host h ON h.singleton=1 JOIN operations o USING(operation_id) JOIN editing_approvals e USING(operation_id) JOIN approvals a USING(operation_id) JOIN runs r ON r.run_id=o.run_id WHERE p.operation_id=? AND p.resource_identity=? AND p.host_epoch=h.epoch AND o.state='dispatching' AND o.owner=? AND o.environment=? AND o.policy=? AND o.requires_approval=1 AND o.tool_version='apply_patch.v1' AND o.argument_digest=? AND e.preview_digest=? AND e.consent_id=? AND e.consent_version=? AND e.read_environment=? AND a.expires_at>? AND a.argument_digest=o.argument_digest AND a.environment=o.environment AND a.policy=o.policy AND r.active=1 AND r.owner=o.owner AND r.policy=o.policy)",
            params![plan.operation_id,edit.resource_identity(),record.owner,record.environment,record.policy,record.preview.argument_digest,plan.preview_digest,record.consent_id,record.consent_version,record.read_environment,now()?], |r|r.get(0),
        ).map_err(JournalError::from)?;
        if !valid {
            return Err(JournalError::ApprovalRequired.into());
        }
        let op = Operation {
            id: &record.operation_id,
            owner: &record.owner,
            run: &record.run,
            invocation: &record.invocation,
            argument_digest: &record.preview.argument_digest,
            tool_version: "apply_patch.v1",
            environment: &record.environment,
            policy_version: &record.policy,
            requires_approval: true,
        };
        self.check_runtime_edit_intent(&op, edit, true)
            .map_err(|_| PatchError::RecoveryRequired)?;
        self.patch_response(&op, &record.args, &record.call_id)?;
        Ok(record)
    }
    fn patch_stage(
        &self,
        token: &PatchDispatch,
        step: i64,
        body: serde_json::Value,
    ) -> Result<(), PatchError> {
        let body = serde_json::to_string(&body).map_err(|_| PatchError::Invalid)?;
        self.connection
            .execute(
                "INSERT INTO editing_publication_steps(operation_id,step,body) VALUES(?,?,?)",
                params![token.plan.operation_id, step, body],
            )
            .map_err(JournalError::from)?;
        Ok(())
    }
    /// Consumes a current-host dispatch token exactly once. Any error is an
    /// uncertain outcome: names and contents are retained for reconciliation.
    /// There is a namespace gap between capture and publication, not a global
    /// atomic content CAS. No automatic restore/delete/overwrite is attempted.
    pub fn publish_patch_dispatch(
        &mut self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        token: PatchDispatch,
    ) -> Result<PublishedPatch, PatchError> {
        self.publish_patch_with_hook(edit, read, token, |_| Ok(()))
    }
    pub(crate) fn publish_patch_with_hook(
        &mut self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        token: PatchDispatch,
        mut hook: impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<PublishedPatch, PatchError> {
        let result = self.publish_patch_inner(edit, read, &token, &mut hook);
        if result.is_err() {
            token
                .approved
                .actual
                ._edit
                .inner
                .revoked
                .store(true, std::sync::atomic::Ordering::Release);
            // If durable reconciliation marking fails, dispatching remains;
            // Journal::open also converts that state to unknown, never replay.
            if let Ok(tx) = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
            {
                let saved = (|| -> Result<(), rusqlite::Error> {
                    tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'operation.unknown' FROM operations WHERE operation_id=? AND state='dispatching'",[&token.plan.operation_id])?;
                    tx.execute("UPDATE operations SET state='unknown' WHERE operation_id=? AND state='dispatching'",[&token.plan.operation_id])?;
                    Ok(())
                })();
                if saved.is_ok() {
                    let _ = tx.commit();
                }
            }
        }
        result
    }
    fn publish_patch_inner(
        &mut self,
        edit: &EditFileGrant,
        read: &WorkspaceGrant,
        token: &PatchDispatch,
        hook: &mut impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<PublishedPatch, PatchError> {
        let record = self.live_patch_dispatch(edit, read, token)?;
        let root = &token.approved.actual._edit.inner.directory;
        let plan = &token.plan;
        let (_, actual, native_name) = super::super::authority::read_source(root, &record.args)?;
        if actual != record.source_identity || native_name != plan.native_target()? {
            return Err(PatchError::VersionChanged);
        }
        let names = components(&plan.path, false)?;
        let (_, parents) = names.split_last().ok_or(PatchError::Invalid)?;
        let name = plan.native_target()?;
        let parent = open_directory(root, parents)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let mut original = parent.open_with(name, &options).map_err(io_error)?;
        if inspected(&parent, &original)? != actual {
            return Err(PatchError::VersionChanged);
        }
        verify_bytes(&mut original, &plan.original_sha256)?;
        self.patch_stage(token, 1, serde_json::json!({"kind":"stage_intent"}))?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        let mut staged = parent
            .open_with(&plan.staged_name, &options)
            .map_err(io_error)?;
        staged
            .write_all(token.approved.content())
            .map_err(io_error)?;
        let security = orange_platform_files::copy_publication_metadata(&original, &staged)
            .map_err(io_error)?;
        staged.sync_all().map_err(io_error)?;
        verify_bytes(&mut staged, &plan.result_sha256)?;
        named_file(&parent, &plan.staged_name, &staged)?;
        sync_directory(&parent)?;
        self.patch_stage(
            token,
            2,
            serde_json::json!({"kind":"staged","security_digest":hex::encode(security.digest())}),
        )?;
        hook(2)?;
        self.live_patch_dispatch(edit, read, token)?;
        let (_, current_identity, current_name) =
            super::super::authority::read_source(root, &record.args)?;
        if current_identity != actual
            || current_name != plan.native_target()?
            || inspected(&parent, &original)? != actual
            || directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)?
        {
            return Err(PatchError::VersionChanged);
        }
        named_file(&parent, name, &original)?;
        self.patch_stage(token, 3, serde_json::json!({"kind":"capture_intent"}))?;
        move_without_replace(&parent, name, &plan.backup_name)?;
        hook(3)?;
        sync_directory(&parent)?;
        named_file(&parent, &plan.backup_name, &original)?;
        if !actual.matches_after_rename(&inspected(&parent, &original)?) {
            return Err(PatchError::VersionChanged);
        }
        verify_bytes(&mut original, &plan.original_sha256)?;
        self.patch_stage(token, 4, serde_json::json!({"kind":"captured"}))?;
        hook(4)?;
        self.live_patch_dispatch(edit, read, token)?;
        if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
            return Err(PatchError::VersionChanged);
        }
        named_file(&parent, &plan.staged_name, &staged)?;
        named_file(&parent, &plan.backup_name, &original)?;
        verify_bytes(&mut original, &plan.original_sha256)?;
        verify_bytes(&mut staged, &plan.result_sha256)?;
        if orange_platform_files::security_metadata(&staged).map_err(io_error)? != security {
            return Err(PatchError::VersionChanged);
        }
        self.patch_stage(token, 5, serde_json::json!({"kind":"publish_intent"}))?;
        move_without_replace(&parent, &plan.staged_name, name)?;
        hook(5)?;
        sync_directory(&parent)?;
        self.patch_stage(token, 6, serde_json::json!({"kind":"published"}))?;
        hook(6)?;
        named_file(&parent, name, &staged)?;
        named_file(&parent, &plan.backup_name, &original)?;
        verify_bytes(&mut staged, &plan.result_sha256)?;
        verify_bytes(&mut original, &plan.original_sha256)?;
        if !actual.matches_after_rename(&inspected(&parent, &original)?)
            || orange_platform_files::security_metadata(&staged).map_err(io_error)? != security
            || directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)?
        {
            return Err(PatchError::VersionChanged);
        }
        self.live_patch_dispatch(edit, read, token)?;
        hook(7)?;
        self.live_patch_dispatch(edit, read, token)?;
        named_file(&parent, name, &staged)?;
        named_file(&parent, &plan.backup_name, &original)?;
        verify_bytes(&mut staged, &plan.result_sha256)?;
        verify_bytes(&mut original, &plan.original_sha256)?;
        let retained_identity = inspected(&parent, &original)?;
        let result_identity = inspected(&parent, &staged)?;
        if !actual.matches_after_rename(&retained_identity)
            || orange_platform_files::security_metadata(&staged).map_err(io_error)? != security
            || directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)?
        {
            return Err(PatchError::VersionChanged);
        }
        let receipt = PublishedPatch {
            path: plan.path.clone(),
            original_sha256: plan.original_sha256.clone(),
            result_sha256: plan.result_sha256.clone(),
            bytes: token.approved.content().len().to_string(),
            retained_original: plan.backup_name.clone(),
        };
        self.check_runtime_edit_output(&record.owner, &record.run, &plan.operation_id, &receipt)
            .map_err(|_| PatchError::RecoveryRequired)?;
        let model =
            serde_json::to_string(&receipt.model_value()).map_err(|_| PatchError::Invalid)?;
        let body = serde_json::to_string(&receipt).map_err(|_| PatchError::Invalid)?;
        let digest =
            request_digest("published-patch.v1", &receipt).map_err(|_| PatchError::Invalid)?;
        let edge = super::super::lineage::VersionEdge {
            operation_id: plan.operation_id.clone(),
            owner: record.owner.clone(),
            run: record.run.clone(),
            policy: record.policy.clone(),
            read_environment: record.read_environment.clone(),
            resource: read.resource_identity().into(),
            path: plan.path.clone(),
            preview_digest: plan.preview_digest.clone(),
            receipt_digest: digest.clone(),
            original_sha256: plan.original_sha256.clone(),
            result_sha256: plan.result_sha256.clone(),
            retained_name: plan.backup_name.clone(),
            before: actual,
            retained: retained_identity,
            after: result_identity,
        };
        let edge_body = serde_json::to_string(&edge).map_err(|_| PatchError::Invalid)?;
        let edge_digest = edge.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        tx.execute(
            "INSERT INTO editing_publication_steps(operation_id,step,body) VALUES(?,7,?)",
            params![plan.operation_id, body],
        )
        .map_err(JournalError::from)?;
        if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![digest,body,model,plan.operation_id]).map_err(JournalError::from)? != 1 { return Err(JournalError::Transition.into()); }
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.succeeded')",
            params![record.run, plan.operation_id],
        )
        .map_err(JournalError::from)?;
        let event_seq = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO patch_version_edges(operation_id,event_seq,body,digest) VALUES(?,?,?,?)",
            params![plan.operation_id, event_seq, edge_body, edge_digest],
        )
        .map_err(JournalError::from)?;
        if !token.has_live_directory_access() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests;
