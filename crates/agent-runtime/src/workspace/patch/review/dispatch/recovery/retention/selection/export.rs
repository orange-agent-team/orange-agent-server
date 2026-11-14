//! Bounded export capture only. Destination consent, exact approval and durable
//! publication are separate: capturing this artifact never writes or deletes.
use super::*;
mod publication;
use crate::workspace::patch::authority;
use crate::workspace::{CancellationGuard, Permit, READ_SLOTS, directory_identity, open_directory};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt, OsMetadataExt};
use cap_std::fs::{Dir, OpenOptions};
pub use publication::{
    ExportFileObservation, ExportInspectionGrant, ExportInspectionReceipt, ExportInspectionView,
    ExportQuiescence, PreparedRetentionExport, RetentionExportDestination, RetentionExportOutcome,
    RetentionExportReceipt, RetentionExportReview,
};
use std::io::Read;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionExportChoice {
    Original,
    Staged,
    Both,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionExportEntrySummary {
    pub role: String,
    pub bytes: String,
    pub sha256: String,
    pub security_sha256: String,
    pub reviewed_original_inode: bool,
    pub matches_reviewed_original: bool,
    pub matches_reviewed_result: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionExportPreview {
    pub format: String,
    pub operation_id: String,
    pub path: String,
    pub choice: RetentionExportChoice,
    pub entries: Vec<RetentionExportEntrySummary>,
    pub artifact_bytes: String,
    pub artifact_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportEntry {
    summary: RetentionExportEntrySummary,
    content_base64: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportDocument {
    format: String,
    operation_id: String,
    source_path: String,
    choice: RetentionExportChoice,
    reviewed_original_sha256: String,
    reviewed_result_sha256: String,
    entries: Vec<ExportEntry>,
}
/// Not serializable/debuggable: raw retained bytes never enter model history or
/// an ordinary Host DTO. The eventual trusted publisher must retain this lease.
pub struct RetentionExportCapture {
    preview: RetentionExportPreview,
    body: Vec<u8>,
    permit: Permit,
}
impl RetentionExportCapture {
    pub fn preview(&self) -> Result<&RetentionExportPreview, PatchError> {
        self.check()?;
        Ok(&self.preview)
    }
    /// Trusted export publisher only, after separate destination consent and
    /// approval. This read accessor itself confers no write or delete authority.
    pub fn encoded_bytes(&self) -> Result<&[u8], PatchError> {
        self.check()?;
        Ok(&self.body)
    }
    fn check(&self) -> Result<(), PatchError> {
        if self.permit.inner.is_revoked() {
            Err(WorkspaceError::Revoked.into())
        } else {
            Ok(())
        }
    }
}
fn io(error: std::io::Error) -> PatchError {
    WorkspaceError::Io(error).into()
}
fn read_entry(
    parent: &Dir,
    name: &str,
    role: &str,
    observed: &RecoveryFileObservation,
) -> Result<Option<ExportEntry>, PatchError> {
    let RecoveryFileObservation::Regular {
        bytes,
        sha256,
        security_sha256,
        object_digest,
        reviewed_original_inode,
        matches_original_content,
        matches_result_content,
        ..
    } = observed
    else {
        return match observed {
            RecoveryFileObservation::Missing => Ok(None),
            _ => Err(PatchError::RecoveryRequired),
        };
    };
    let named = parent.symlink_metadata(name).map_err(io)?;
    if !named.is_file() || named.nlink() != 1 || named.len() > MAX_TEXT as u64 {
        return Err(PatchError::VersionChanged);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = parent.open_with(name, &options).map_err(io)?;
    let identity = authority::identity(parent, &file.metadata().map_err(io)?)?;
    if identity != authority::identity(parent, &named)? {
        return Err(PatchError::VersionChanged);
    }
    let security = orange_platform_files::security_metadata(&file).map_err(io)?;
    let mut secured = identity.clone();
    secured.bind_security(security.digest());
    if request_digest("patch-recovery-object.v1", &secured).map_err(|_| PatchError::Invalid)?
        != *object_digest
        || hex::encode(security.digest()) != *security_sha256
    {
        return Err(PatchError::VersionChanged);
    }
    let mut content = Vec::new();
    (&mut file)
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut content)
        .map_err(io)?;
    if content.len() > MAX_TEXT
        || content.len().to_string() != *bytes
        || hex::encode(sha2::Sha256::digest(&content)) != *sha256
        || identity != authority::identity(parent, &file.metadata().map_err(io)?)?
        || identity != authority::identity(parent, &parent.symlink_metadata(name).map_err(io)?)?
        || security != orange_platform_files::security_metadata(&file).map_err(io)?
    {
        return Err(PatchError::VersionChanged);
    }
    Ok(Some(ExportEntry {
        summary: RetentionExportEntrySummary {
            role: role.into(),
            bytes: bytes.clone(),
            sha256: sha256.clone(),
            security_sha256: security_sha256.clone(),
            reviewed_original_inode: *reviewed_original_inode,
            matches_reviewed_original: *matches_original_content,
            matches_reviewed_result: *matches_result_content,
        },
        content_base64: STANDARD.encode(content),
    }))
}
impl Journal {
    pub async fn capture_retention_export(
        &self,
        grant: &PatchRetentionGrant,
        choice: RetentionExportChoice,
    ) -> Result<RetentionExportCapture, PatchError> {
        self.validate_retention_selection(grant)?;
        let observed = self.inspect_selected_retention(grant).await?;
        let snapshot = observed.snapshot()?.clone();
        drop(observed);
        let slot = tokio::time::timeout(
            grant.read.inner.deadline,
            READ_SLOTS.clone().acquire_owned(),
        )
        .await
        .map_err(|_| WorkspaceError::Timeout)?
        .map_err(|_| WorkspaceError::Interrupted)?;
        let permit = grant
            .read
            .permit(
                grant.read.owner(),
                grant.read.environment(),
                grant.read.policy(),
            )
            .await?;
        let mut cancellation = CancellationGuard {
            inner: permit.inner.clone(),
            finished: false,
        };
        let work =
            tokio::task::spawn_blocking(move || -> Result<RetentionExportCapture, PatchError> {
                let _slot = slot;
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let root = &permit.inner.directory;
                let parts = components(&snapshot.journal.plan.path, false)?;
                let (_, parents) = parts.split_last().ok_or(PatchError::Invalid)?;
                let parent = open_directory(root, parents)?;
                let plan = &snapshot.journal.plan;
                let mut entries = Vec::new();
                if matches!(
                    choice,
                    RetentionExportChoice::Original | RetentionExportChoice::Both
                ) && let Some(entry) =
                    read_entry(&parent, &plan.backup_name, "original", &snapshot.original)?
                {
                    entries.push(entry);
                }
                if matches!(
                    choice,
                    RetentionExportChoice::Staged | RetentionExportChoice::Both
                ) && let Some(entry) =
                    read_entry(&parent, &plan.staged_name, "staged", &snapshot.staged)?
                {
                    entries.push(entry);
                }
                if entries.is_empty() {
                    return Err(PatchError::Invalid);
                }
                if directory_identity(&open_directory(root, parents)?)?
                    != directory_identity(&parent)?
                {
                    return Err(WorkspaceError::EnvironmentChanged.into());
                }
                let preview_entries = entries.iter().map(|e| e.summary.clone()).collect();
                let document = ExportDocument {
                    format: "orange-retained-files.v1".into(),
                    operation_id: plan.operation_id.clone(),
                    source_path: plan.path.clone(),
                    choice,
                    reviewed_original_sha256: plan.original_sha256.clone(),
                    reviewed_result_sha256: plan.result_sha256.clone(),
                    entries,
                };
                let body = serde_json::to_vec(&document).map_err(|_| PatchError::Invalid)?;
                if body.len() > 3 * MAX_TEXT {
                    return Err(PatchError::Limit);
                }
                let preview = RetentionExportPreview {
                    format: document.format,
                    operation_id: document.operation_id,
                    path: document.source_path,
                    choice,
                    entries: preview_entries,
                    artifact_bytes: body.len().to_string(),
                    artifact_sha256: hex::encode(sha2::Sha256::digest(&body)),
                };
                let capture = RetentionExportCapture {
                    preview,
                    body,
                    permit,
                };
                capture.check()?;
                Ok(capture)
            });
        let result = tokio::time::timeout(grant.read.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        let capture = result?;
        self.validate_retention_selection(grant)?;
        capture.check()?;
        Ok(capture)
    }
}

/// Validate only the format and internal content integrity of an exported
/// document. Unkeyed checksums cannot prove who produced it; no file authority
/// or import/write action is created by this offline verifier.
pub fn verify_retention_export(body: &[u8]) -> Result<RetentionExportPreview, PatchError> {
    if body.len() > 3 * MAX_TEXT {
        return Err(PatchError::Limit);
    }
    let document: ExportDocument = serde_json::from_slice(body).map_err(|_| PatchError::Invalid)?;
    let hash = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if document.format != "orange-retained-files.v1"
        || !valid_id(&document.operation_id)
        || !hash(&document.reviewed_original_sha256)
        || !hash(&document.reviewed_result_sha256)
        || document.entries.is_empty()
        || document.entries.len() > 2
    {
        return Err(PatchError::Invalid);
    }
    components(&document.source_path, false)?;
    let mut roles = std::collections::BTreeSet::new();
    for entry in &document.entries {
        let info = &entry.summary;
        let allowed = match document.choice {
            RetentionExportChoice::Original => info.role == "original",
            RetentionExportChoice::Staged => info.role == "staged",
            RetentionExportChoice::Both => ["original", "staged"].contains(&info.role.as_str()),
        };
        if !allowed
            || !roles.insert(&info.role)
            || !hash(&info.sha256)
            || !hash(&info.security_sha256)
        {
            return Err(PatchError::Invalid);
        }
        let bytes = STANDARD
            .decode(&entry.content_base64)
            .map_err(|_| PatchError::Invalid)?;
        if bytes.len() > MAX_TEXT
            || bytes.len().to_string() != info.bytes
            || hex::encode(sha2::Sha256::digest(&bytes)) != info.sha256
            || info.matches_reviewed_original != (info.sha256 == document.reviewed_original_sha256)
            || info.matches_reviewed_result != (info.sha256 == document.reviewed_result_sha256)
        {
            return Err(PatchError::Invalid);
        }
    }
    Ok(RetentionExportPreview {
        format: document.format,
        operation_id: document.operation_id,
        path: document.source_path,
        choice: document.choice,
        entries: document.entries.into_iter().map(|e| e.summary).collect(),
        artifact_bytes: body.len().to_string(),
        artifact_sha256: hex::encode(sha2::Sha256::digest(body)),
    })
}
