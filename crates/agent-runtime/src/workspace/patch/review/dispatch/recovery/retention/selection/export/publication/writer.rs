//! The writer owns its original lock until all filesystem effects end. No PID
//! inference, lock replacement, forced unlock or schema18 evidence fabrication.
use super::*;
use rusqlite::Transaction;
use std::fs::{File, OpenOptions as StdOpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    export_id: String,
    device: String,
    inode: String,
    nonce_sha256: String,
}
impl Binding {
    fn digest(&self) -> Result<String, PatchError> {
        request_digest("export-writer-binding.v1", self).map_err(|_| PatchError::Invalid)
    }
}
pub(super) struct WriterLease {
    file: File,
    path: std::path::PathBuf,
    binding: Binding,
}
// Deliberately no explicit unlock on Drop: any inherited open-file description
// must keep the lease busy until its last holder closes. No clone is exposed.
impl WriterLease {
    pub(super) fn check(&self) -> Result<(), PatchError> {
        validate(&self.path, &self.file, &self.binding)
    }
}
/// Exclusive evidence for this app's registered export worker only. External
/// writers and target namespace changes require separate disposal checks.
pub struct ExportQuiescence {
    lease: WriterLease,
    read: Permit,
}
impl ExportQuiescence {
    /// Only meaningful while this object is retained; it grants no deletion.
    pub fn check(&self) -> Result<(), PatchError> {
        if self.read.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        self.lease.check()
    }
    pub fn export_id(&self) -> Result<&str, PatchError> {
        self.check()?;
        Ok(&self.lease.binding.export_id)
    }
}
fn path(root: &Path, id: &str) -> Result<std::path::PathBuf, PatchError> {
    if uuid::Uuid::parse_str(id)
        .map_err(|_| PatchError::Invalid)?
        .to_string()
        != id
    {
        return Err(PatchError::Invalid);
    }
    Ok(root.join(format!(".export-writer-{id}.lease")))
}
fn private_file(path: &Path, file: &File) -> Result<std::fs::Metadata, PatchError> {
    let named = std::fs::symlink_metadata(path).map_err(io)?;
    let opened = file.metadata().map_err(io)?;
    if !named.is_file()
        || named.file_type().is_symlink()
        || named.nlink() != 1
        || named.mode() & 0o077 != 0
        || !opened.is_file()
        || opened.nlink() != 1
        || opened.mode() & 0o077 != 0
        || named.dev() != opened.dev()
        || named.ino() != opened.ino()
        || opened.len() != 32
    {
        return Err(PatchError::RecoveryRequired);
    }
    Ok(opened)
}
fn validate(path: &Path, file: &File, binding: &Binding) -> Result<(), PatchError> {
    use std::os::unix::fs::FileExt;
    let metadata = private_file(path, file)?;
    let mut bytes = [0; 32];
    file.read_exact_at(&mut bytes, 0).map_err(io)?;
    if metadata.dev().to_string() != binding.device
        || metadata.ino().to_string() != binding.inode
        || hex::encode(sha2::Sha256::digest(bytes)) != binding.nonce_sha256
    {
        return Err(PatchError::RecoveryRequired);
    }
    let after = private_file(path, file)?;
    if metadata.dev() != after.dev() || metadata.ino() != after.ino() {
        return Err(PatchError::RecoveryRequired);
    }
    Ok(())
}
fn lock(file: &File) -> Result<(), PatchError> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => PatchError::RecoveryRequired,
        std::fs::TryLockError::Error(error) => io(error),
    })
}
pub(super) fn create(
    root: &Path,
    id: &str,
    tx: &Transaction<'_>,
) -> Result<WriterLease, PatchError> {
    let path = path(root, id)?;
    let mut file = StdOpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(io)?;
    lock(&file)?;
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    file.write_all(&bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    File::open(root).map_err(io)?.sync_all().map_err(io)?;
    let meta = private_file(&path, &file)?;
    let binding = Binding {
        export_id: id.into(),
        device: meta.dev().to_string(),
        inode: meta.ino().to_string(),
        nonce_sha256: hex::encode(sha2::Sha256::digest(bytes)),
    };
    validate(&path, &file, &binding)?;
    tx.execute(
        "INSERT INTO export_writer_bindings(export_id,body,digest) VALUES(?,?,?)",
        params![
            id,
            serde_json::to_string(&binding).map_err(|_| PatchError::Invalid)?,
            binding.digest()?
        ],
    )
    .map_err(JournalError::from)?;
    Ok(WriterLease {
        file,
        path,
        binding,
    })
}
impl Journal {
    /// Fresh destination inspection authority plus an exclusive lock on the
    /// original registered writer inode. Missing legacy evidence is refusal.
    pub async fn acquire_export_quiescence(
        &self,
        grant: &ExportInspectionGrant,
    ) -> Result<ExportQuiescence, PatchError> {
        let (record, outcome) = self.validate_export_inspection(grant)?;
        if !["unknown", "succeeded"].contains(&outcome.state.as_str()) {
            return Err(PatchError::RecoveryRequired);
        }
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT body,digest FROM export_writer_bindings WHERE export_id=?",
                [&outcome.export_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(JournalError::from)?;
        let (body, digest) = row.ok_or(PatchError::RecoveryRequired)?;
        if body.len() > 4096 {
            return Err(PatchError::Limit);
        }
        let binding: Binding = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if binding.export_id != outcome.export_id || binding.digest()? != digest {
            return Err(PatchError::Invalid);
        }
        let path = path(&self.private_root, &outcome.export_id)?;
        let file = StdOpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(io)?;
        validate(&path, &file, &binding)?;
        lock(&file)?;
        validate(&path, &file, &binding)?;
        let read = grant
            .read
            .permit(&record.owner, grant.read.environment(), grant.read.policy())
            .await?;
        self.validate_export_inspection(grant)?;
        let proof = ExportQuiescence {
            lease: WriterLease {
                file,
                path,
                binding,
            },
            read,
        };
        proof.check()?;
        Ok(proof)
    }
}
