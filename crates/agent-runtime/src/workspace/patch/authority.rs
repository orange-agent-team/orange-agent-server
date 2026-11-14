//! Explicit edit authority and real-source preview. No publication is performed.
mod consent;
use super::*;
use crate::workspace::{
    CancellationGuard, Permit, READ_SLOTS, WorkspaceGrant, directory_identity, open_directory,
};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt, OsMetadataExt};
use cap_std::fs::{Dir, OpenOptions};
pub use consent::{EditConsentView, EditRevoker};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// A trusted host constructs this only after separate edit consent. Neither
/// readonly nor create-only grants can be converted into this type.
#[derive(Clone)]
pub struct EditFileGrant {
    grant: WorkspaceGrant,
    environment: String,
    consent_id: String,
    version: i64,
    epoch: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceIdentity {
    parent_device: u64,
    parent_inode: u64,
    device: u64,
    inode: u64,
    created_nanos: String,
    modified_nanos: String,
    change_seconds: i64,
    change_nanos: i64,
    bytes: u64,
    mode: u32,
    security_digest: String,
}
pub struct AuthorizedPatchPreview {
    prepared: PreparedPatch,
    pub(super) args: PatchArgs,
    binding_digest: String,
    pub(super) identity: SourceIdentity,
    pub(super) native_name: String,
    pub(super) _read: Permit,
    pub(super) _edit: Permit,
}
impl AuthorizedPatchPreview {
    pub fn preview(&self) -> &PatchPreview {
        self.prepared.preview()
    }
    pub fn content(&self) -> &[u8] {
        self.prepared.content()
    }
    pub fn source_binding_digest(&self) -> &str {
        &self.binding_digest
    }
    pub fn is_authorized(&self) -> bool {
        !self._read.inner.is_revoked() && !self._edit.inner.is_revoked()
    }
}
impl EditFileGrant {
    pub fn open(
        owner: &str,
        consent: &str,
        policy: &str,
        root: &Path,
        private_roots: &[PathBuf],
    ) -> Result<Self, PatchError> {
        let grant = WorkspaceGrant::open(owner, consent, policy, root, private_roots)?;
        if !grant.has_persistent_identity() {
            return Err(WorkspaceError::PersistentIdentityUnavailable.into());
        }
        let environment = request_digest(
            "edit-existing-workspace.v1",
            &(consent, grant.environment()),
        )
        .map_err(|_| PatchError::Invalid)?;
        Ok(Self {
            grant,
            environment,
            consent_id: consent.into(),
            version: 0,
            epoch: None,
        })
    }
    pub fn consent_id(&self) -> &str {
        &self.consent_id
    }
    pub fn consent_version(&self) -> Option<String> {
        self.epoch.map(|_| self.version.to_string())
    }
    pub fn environment(&self) -> &str {
        &self.environment
    }
    pub fn resource_identity(&self) -> &str {
        self.grant.resource_identity()
    }
    pub fn owner(&self) -> &str {
        self.grant.owner()
    }
    pub fn policy(&self) -> &str {
        self.grant.policy()
    }
    /// Re-read the actual object under the same live authorities. A matching
    /// content hash alone does not adopt a replacement inode or changed mode.
    pub async fn revalidate(
        &self,
        read: &WorkspaceGrant,
        prior: &AuthorizedPatchPreview,
    ) -> Result<AuthorizedPatchPreview, PatchError> {
        if prior._read.inner.is_revoked() || prior._edit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        if prior._read.inner.environment != read.environment()
            || prior._edit.inner.environment != self.grant.environment()
        {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let current = self.preview(read, prior.args.clone()).await?;
        if current.identity != prior.identity || current.binding_digest != prior.binding_digest {
            return Err(PatchError::VersionChanged);
        }
        Ok(current)
    }
    pub fn request_revoke(&self) {
        self.grant.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.grant.wait_drained().await;
    }
    /// Both capabilities stay leased until this returned preview is persisted
    /// or discarded. This is not a durable consent/approval and does not edit.
    pub async fn preview(
        &self,
        read: &WorkspaceGrant,
        args: PatchArgs,
    ) -> Result<AuthorizedPatchPreview, PatchError> {
        args.validate()?;
        if read.owner() != self.owner()
            || read.policy() != self.policy()
            || read.resource_identity() != self.resource_identity()
        {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let slot = READ_SLOTS
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkspaceError::Interrupted)?;
        let read = read
            .permit(self.owner(), read.environment(), self.policy())
            .await?;
        let edit = self
            .grant
            .permit(self.owner(), self.grant.environment(), self.policy())
            .await?;
        let mut cancellation = CancellationGuard {
            inner: edit.inner.clone(),
            finished: false,
        };
        let environment = self.environment.clone();
        let work =
            tokio::task::spawn_blocking(move || -> Result<AuthorizedPatchPreview, PatchError> {
                let _slot = slot;
                if read.inner.is_revoked() || edit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let (original, identity, native_name) = read_source(&edit.inner.directory, &args)?;
                let prepared = prepare(&args, &original)?;
                if read.inner.is_revoked() || edit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let binding_digest = request_digest(
                    "patch-source-binding.v2",
                    &(
                        &environment,
                        &read.inner.environment,
                        &identity,
                        &native_name,
                        &prepared.preview,
                    ),
                )
                .map_err(|_| PatchError::Invalid)?;
                Ok(AuthorizedPatchPreview {
                    args,
                    prepared,
                    binding_digest,
                    identity,
                    native_name,
                    _read: read,
                    _edit: edit,
                })
            });
        let result = tokio::time::timeout(self.grant.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        let result = result?;
        if !result.is_authorized() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(result)
    }
}
pub(super) fn identity(
    parent: &Dir,
    metadata: &cap_std::fs::Metadata,
) -> Result<SourceIdentity, PatchError> {
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.ino() == 0 {
        return Err(WorkspaceError::Forbidden.into());
    }
    let (parent_device, parent_inode) = directory_identity(parent)?;
    fn stamp(value: cap_std::time::SystemTime) -> String {
        match value.into_std().duration_since(UNIX_EPOCH) {
            Ok(value) => format!("+{}", value.as_nanos()),
            Err(error) => format!("-{}", error.duration().as_nanos()),
        }
    }
    let created_nanos = stamp(
        metadata
            .created()
            .map_err(|_| WorkspaceError::PersistentIdentityUnavailable)?,
    );
    let modified_nanos = stamp(metadata.modified().map_err(WorkspaceError::Io)?);
    Ok(SourceIdentity {
        parent_device,
        parent_inode,
        device: metadata.dev(),
        inode: metadata.ino(),
        created_nanos,
        modified_nanos,
        change_seconds: metadata.ctime(),
        change_nanos: metadata.ctime_nsec(),
        bytes: metadata.len(),
        mode: metadata.mode(),
        security_digest: String::new(),
    })
}
pub(super) fn read_source(
    root: &Dir,
    args: &PatchArgs,
) -> Result<(Vec<u8>, SourceIdentity, String), PatchError> {
    let names = components(&args.path, false)?;
    let (name, parents) = names.split_last().ok_or(PatchError::Invalid)?;
    let parent = open_directory(root, parents)?;
    let named = identity(
        &parent,
        &parent.symlink_metadata(name).map_err(WorkspaceError::Io)?,
    )?;
    if named.bytes > MAX_TEXT as u64 {
        return Err(PatchError::Limit);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = parent
        .open_with(name, &options)
        .map_err(WorkspaceError::Io)?;
    let mut opened = identity(&parent, &file.metadata().map_err(WorkspaceError::Io)?)?;
    if named != opened {
        return Err(PatchError::VersionChanged);
    }
    let security = orange_platform_files::security_metadata(&file).map_err(WorkspaceError::Io)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(WorkspaceError::Io)?;
    if bytes.len() > MAX_TEXT {
        return Err(PatchError::Limit);
    }
    if identity(&parent, &file.metadata().map_err(WorkspaceError::Io)?)? != opened
        || identity(
            &parent,
            &parent.symlink_metadata(name).map_err(WorkspaceError::Io)?,
        )? != opened
        || opened.bytes != bytes.len() as u64
    {
        return Err(PatchError::VersionChanged);
    }
    // Re-resolve parents without following links; a moved/replaced parent does
    // not validate merely because the held directory descriptor is still open.
    if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
        return Err(PatchError::VersionChanged);
    }
    if orange_platform_files::security_metadata(&file).map_err(WorkspaceError::Io)? != security {
        return Err(PatchError::VersionChanged);
    }
    let native_name = native_entry_name(&parent, &file, &opened)?;
    if identity(&parent, &file.metadata().map_err(WorkspaceError::Io)?)? != opened
        || identity(
            &parent,
            &parent.symlink_metadata(name).map_err(WorkspaceError::Io)?,
        )? != opened
    {
        return Err(PatchError::VersionChanged);
    }
    opened.security_digest = hex::encode(security.digest());
    Ok((bytes, opened, native_name))
}
fn native_entry_name(
    parent: &Dir,
    file: &cap_std::fs::File,
    expected: &SourceIdentity,
) -> Result<String, PatchError> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::ffi::OsStrExt;
        // Only use the leaf spelling from the live descriptor. Never open or
        // expose the ambient path returned by F_GETPATH.
        let native = rustix::fs::getpath(file).map_err(|e| WorkspaceError::Io(e.into()))?;
        let name = Path::new(std::ffi::OsStr::from_bytes(native.as_bytes()))
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(WorkspaceError::Forbidden)?
            .to_owned();
        components(&name, false)?;
        if identity(
            parent,
            &parent.symlink_metadata(&name).map_err(WorkspaceError::Io)?,
        )? != *expected
        {
            return Err(PatchError::VersionChanged);
        }
        Ok(name)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = file;
        for (count, entry) in parent.entries().map_err(WorkspaceError::Io)?.enumerate() {
            if count >= 10000 {
                return Err(PatchError::Limit);
            }
            let name = entry.map_err(WorkspaceError::Io)?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let metadata = parent.symlink_metadata(name).map_err(WorkspaceError::Io)?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.dev() != expected.device
                || metadata.ino() != expected.inode
            {
                continue;
            }
            components(name, false)?;
            if identity(parent, &metadata)? != *expected {
                return Err(PatchError::VersionChanged);
            }
            return Ok(name.into());
        }
        Err(PatchError::VersionChanged)
    }
}
impl SourceIdentity {
    #[cfg(target_vendor = "apple")]
    pub(super) fn object_key(&self) -> crate::workspace::historical::ObjectKey {
        crate::workspace::historical::ObjectKey {
            device: self.device,
            inode: self.inode,
            created: self.created_nanos.clone(),
        }
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn same_inode(&self, other: &Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.created_nanos == other.created_nanos
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn same_parent(&self, parent: &Dir) -> Result<bool, PatchError> {
        Ok(directory_identity(parent)? == (self.parent_device, self.parent_inode))
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn bind_security(&mut self, digest: [u8; 32]) {
        self.security_digest = hex::encode(digest);
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn security_digest(&self) -> &str {
        &self.security_digest
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn matches_after_rename(&self, current: &Self) -> bool {
        let mut current = current.clone();
        current.change_seconds = self.change_seconds;
        current.change_nanos = self.change_nanos;
        self == &current
    }
}
#[cfg(test)]
mod tests;
