//! Read-only reconciliation snapshot under an explicit current read capability.
//! Protected recovery names come solely from the immutable owned journal plan.
use super::*;
use crate::workspace::patch::authority;
use crate::workspace::{CancellationGuard, Permit, READ_SLOTS, directory_identity, open_directory};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt, OsMetadataExt};
use cap_std::fs::{Dir, OpenOptions};
use std::io::Read;
use std::path::Path;

/// A fresh native read selection for one recovery operation. This type exposes
/// no ordinary WorkspaceGrant and cannot be used to start an Agent or edit.
pub struct PatchInspectionGrant {
    pub(super) read: WorkspaceGrant,
    pub(super) operation: String,
    run: String,
    epoch: i64,
}
impl PatchInspectionGrant {
    pub fn request_revoke(&self) {
        self.read.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.read.wait_drained().await;
    }
}

impl Journal {
    /// Trusted host only after an explicit recovery-read directory selection.
    /// Cancelled Runs remain inspectable by fresh consent; no old scope is
    /// restored and no normal execution quarantine is removed.
    pub fn authorize_patch_inspection(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        root: &Path,
    ) -> Result<PatchInspectionGrant, PatchError> {
        if ![owner, run, operation].iter().all(|value| valid_id(value)) {
            return Err(PatchError::Invalid);
        }
        let row: Option<(String,String)> = self.connection.query_row(
            "SELECT o.policy,p.resource_identity FROM operations o JOIN editing_publications p USING(operation_id)
             JOIN runtime_runs r ON r.run_id=o.run_id
             WHERE o.owner=? AND o.run_id=? AND o.operation_id=? AND o.tool_version='apply_patch.v1'
             AND o.state='unknown' AND NOT EXISTS(SELECT 1 FROM patch_recovery_resolutions x WHERE x.operation_id=o.operation_id)",
            params![owner,run,operation], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional().map_err(JournalError::from)?;
        let (policy, resource) = row.ok_or(WorkspaceError::Forbidden)?;
        let mut private = self.extra_private_roots.clone();
        private.push(self.private_root.clone());
        let read = WorkspaceGrant::open(
            owner,
            &uuid::Uuid::new_v4().to_string(),
            &policy,
            root,
            &private,
        )?;
        if !read.has_persistent_identity() || read.resource_identity() != resource {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let epoch = self
            .connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        let result = PatchInspectionGrant {
            read,
            operation: operation.into(),
            run: run.into(),
            epoch,
        };
        self.validate_patch_inspection(&result)?;
        self.connection.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.inspection_consent')",params![run,operation]).map_err(JournalError::from)?;
        Ok(result)
    }
    pub(super) fn validate_patch_inspection(
        &self,
        grant: &PatchInspectionGrant,
    ) -> Result<(), PatchError> {
        let epoch: i64 = self
            .connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if grant.read.is_revoked() || grant.epoch != epoch {
            return Err(WorkspaceError::Revoked.into());
        }
        let valid: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id)
             WHERE o.owner=? AND o.run_id=? AND o.operation_id=? AND o.tool_version='apply_patch.v1'
             AND o.state='unknown' AND o.policy=? AND p.resource_identity=?)",
            params![grant.read.owner(),grant.run,grant.operation,grant.read.policy(),grant.read.resource_identity()],|r|r.get(0),
        ).map_err(JournalError::from)?;
        if !valid {
            return Err(WorkspaceError::Forbidden.into());
        }
        Ok(())
    }
    pub async fn inspect_selected_patch(
        &self,
        grant: &PatchInspectionGrant,
    ) -> Result<PatchRecoveryReceipt, PatchError> {
        self.validate_patch_inspection(grant)?;
        let receipt = self
            .inspect_patch_recovery(grant.read.owner(), &grant.operation, &grant.read)
            .await?;
        self.validate_patch_inspection(grant)?;
        Ok(receipt)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryFileObservation {
    Missing,
    Refused {
        reason: String,
    },
    Regular {
        bytes: String,
        sha256: String,
        security_sha256: String,
        object_digest: String,
        reviewed_original_inode: bool,
        matches_original_content: bool,
        matches_result_content: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchRecoverySnapshot {
    pub journal: PublicationRecoveryView,
    pub target: RecoveryFileObservation,
    pub staged: RecoveryFileObservation,
    pub original: RecoveryFileObservation,
}
pub struct PatchRecoveryReceipt {
    snapshot: PatchRecoverySnapshot,
    permit: Permit,
}
impl PatchRecoveryReceipt {
    /// Keep this receipt until presentation/persistence ends. Revocation blocks
    /// any subsequent snapshot access; drain waits for this receipt to drop.
    pub fn snapshot(&self) -> Result<&PatchRecoverySnapshot, PatchError> {
        if self.permit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(&self.snapshot)
    }
}
fn io_error(error: std::io::Error) -> PatchError {
    WorkspaceError::Io(error).into()
}
fn check_name(name: &str, prefix: &str) -> Result<(), PatchError> {
    let suffix = name.strip_prefix(prefix).ok_or(PatchError::Invalid)?;
    let id = uuid::Uuid::parse_str(suffix).map_err(|_| PatchError::Invalid)?;
    if id.to_string() != suffix {
        return Err(PatchError::Invalid);
    }
    Ok(())
}
pub(super) fn observe(
    parent: &Dir,
    name: &str,
    reviewed: &SourceIdentity,
    plan: &PublicationPlan,
    capture: Option<&crate::workspace::patch::capture::CaptureEvidence>,
) -> Result<RecoveryFileObservation, PatchError> {
    let named = match parent.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RecoveryFileObservation::Missing);
        }
        Err(error) => return Err(io_error(error)),
    };
    if !named.is_file() || named.nlink() != 1 {
        return Ok(RecoveryFileObservation::Refused {
            reason: "not_single_link_regular_file".into(),
        });
    }
    if named.len() > MAX_TEXT as u64 {
        return Ok(RecoveryFileObservation::Refused {
            reason: "size_limit".into(),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = parent.open_with(name, &options).map_err(io_error)?;
    let before = authority::identity(parent, &file.metadata().map_err(io_error)?)?;
    if before != authority::identity(parent, &named)? {
        return Err(PatchError::VersionChanged);
    }
    let security = orange_platform_files::security_metadata(&file).map_err(io_error)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_TEXT
        || bytes.len() as u64 != named.len()
        || before != authority::identity(parent, &file.metadata().map_err(io_error)?)?
        || before != authority::identity(parent, &parent.symlink_metadata(name).map_err(io_error)?)?
        || security != orange_platform_files::security_metadata(&file).map_err(io_error)?
    {
        return Err(PatchError::VersionChanged);
    }
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    let mut secured_identity = before.clone();
    secured_identity.bind_security(security.digest());
    let object_digest = request_digest("patch-recovery-object.v1", &secured_identity)
        .map_err(|_| PatchError::Invalid)?;
    Ok(RecoveryFileObservation::Regular {
        bytes: bytes.len().to_string(),
        security_sha256: hex::encode(security.digest()),
        object_digest,
        reviewed_original_inode: reviewed.same_inode(&before)
            && capture.cloned().unwrap_or_default().accepts_security(
                reviewed,
                &security,
                name == plan.backup_name,
            ),
        matches_original_content: sha256 == plan.original_sha256,
        matches_result_content: sha256 == plan.result_sha256,
        sha256,
    })
}
impl Journal {
    /// A read capability is required even when the journal proves ownership.
    /// A fresh explicit capability for the same owner/policy/physical directory
    /// may inspect unknown recovery files; it grants no editing or replay right.
    pub async fn inspect_patch_recovery(
        &self,
        owner: &str,
        operation: &str,
        read: &WorkspaceGrant,
    ) -> Result<PatchRecoveryReceipt, PatchError> {
        let journal = self
            .patch_recovery_view(owner, operation)?
            .ok_or(WorkspaceError::Forbidden)?;
        let (resource,body,digest): (String,String,String)=self.connection.query_row(
            "SELECT p.resource_identity,r.body,r.digest FROM editing_publications p JOIN editing_previews r USING(operation_id) WHERE p.operation_id=?",[operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).map_err(JournalError::from)?;
        if body.len() > 512 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if request_digest("patch-review.v1", &record).map_err(|_| PatchError::Invalid)? != digest
            || digest != journal.plan.preview_digest
            || record.operation_id != operation
            || owner != record.owner
            || read.owner() != owner
            || read.policy() != record.policy
            || read.resource_identity() != resource
            || read.is_revoked()
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        check_name(&journal.plan.staged_name, ".orange-agent-tmp-patch-new-")?;
        check_name(&journal.plan.backup_name, ".orange-agent-tmp-patch-old-")?;
        let slot = tokio::time::timeout(read.inner.deadline, READ_SLOTS.clone().acquire_owned())
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        let permit = read
            .permit(owner, read.environment(), read.policy())
            .await?;
        let mut cancellation = CancellationGuard {
            inner: permit.inner.clone(),
            finished: false,
        };
        let work =
            tokio::task::spawn_blocking(move || -> Result<PatchRecoveryReceipt, PatchError> {
                let _slot = slot;
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let root = &permit.inner.directory;
                let parts = components(&journal.plan.path, false)?;
                let (name, parents) = parts.split_last().ok_or(PatchError::Invalid)?;
                let parent = open_directory(root, parents)?;
                if !record.source_identity.same_parent(&parent)? {
                    return Err(WorkspaceError::EnvironmentChanged.into());
                }
                if journal
                    .capture_security
                    .as_ref()
                    .is_some_and(|e| !e.source_bound(&record.source_identity))
                {
                    return Err(PatchError::Invalid);
                }
                let target = observe(
                    &parent,
                    name,
                    &record.source_identity,
                    &journal.plan,
                    journal.capture_security.as_ref(),
                )?;
                let staged = observe(
                    &parent,
                    &journal.plan.staged_name,
                    &record.source_identity,
                    &journal.plan,
                    journal.capture_security.as_ref(),
                )?;
                let original = observe(
                    &parent,
                    &journal.plan.backup_name,
                    &record.source_identity,
                    &journal.plan,
                    journal.capture_security.as_ref(),
                )?;
                if directory_identity(&open_directory(root, parents)?)?
                    != directory_identity(&parent)?
                {
                    return Err(WorkspaceError::EnvironmentChanged.into());
                }
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                Ok(PatchRecoveryReceipt {
                    snapshot: PatchRecoverySnapshot {
                        journal,
                        target,
                        staged,
                        original,
                    },
                    permit,
                })
            });
        let result = tokio::time::timeout(read.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        let receipt = result?;
        receipt.snapshot()?;
        Ok(receipt)
    }
}
