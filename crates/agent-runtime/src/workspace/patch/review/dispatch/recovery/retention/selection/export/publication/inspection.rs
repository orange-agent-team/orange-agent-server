//! Fresh destination-only read capability for an owned export record. Inspecting
//! files neither commits a missing receipt nor restores source/write authority.
use super::*;

pub struct ExportInspectionGrant {
    pub(super) read: WorkspaceGrant,
    run: String,
    operation: String,
    export_id: String,
    review_digest: String,
    epoch: i64,
}
impl ExportInspectionGrant {
    pub fn request_revoke(&self) {
        self.read.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.read.wait_drained().await;
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExportFileObservation {
    Missing,
    Refused {
        reason: String,
    },
    Regular {
        bytes: String,
        sha256: String,
        security_sha256: String,
        object_digest: String,
        matches_approved_artifact: bool,
        valid_export_document: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportInspectionView {
    pub export_id: String,
    pub review_digest: String,
    pub recorded_state: String,
    pub file_name: String,
    pub target: ExportFileObservation,
    pub staged: ExportFileObservation,
}
pub struct ExportInspectionReceipt {
    view: ExportInspectionView,
    permit: Permit,
}
impl ExportInspectionReceipt {
    pub fn view(&self) -> Result<&ExportInspectionView, PatchError> {
        if self.permit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(&self.view)
    }
}
impl Journal {
    fn export_inspection_record(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        id: &str,
    ) -> Result<(Record, RetentionExportOutcome, String), PatchError> {
        let outcome = self
            .retention_export_outcome(owner, run, operation, id)?
            .ok_or(WorkspaceError::Forbidden)?;
        let (body,policy):(String,String)=self.connection.query_row(
            "SELECT e.body,o.policy FROM patch_exports e JOIN operations o ON o.operation_id=e.operation_id WHERE e.export_id=? AND e.owner=? AND o.owner=? AND o.run_id=? AND o.operation_id=?",
            params![id,owner,owner,run,operation],|r|Ok((r.get(0)?,r.get(1)?)),
        ).map_err(JournalError::from)?;
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let suffix = record
            .temporary
            .strip_prefix(".orange-agent-tmp-export-")
            .ok_or(PatchError::Invalid)?;
        if !name_valid(&record.file_name)
            || uuid::Uuid::parse_str(suffix)
                .map_err(|_| PatchError::Invalid)?
                .to_string()
                != suffix
            || record.owner != owner
            || record.run != run
            || record.operation != operation
            || record.digest()? != outcome.review_digest
        {
            return Err(PatchError::Invalid);
        }
        Ok((record, outcome, policy))
    }
    /// Trusted host only after a fresh explicit selection of the export's
    /// destination. The original source need not exist or remain authorized.
    pub fn authorize_export_inspection(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        export_id: &str,
        root: &Path,
    ) -> Result<ExportInspectionGrant, PatchError> {
        let (record, outcome, policy) =
            self.export_inspection_record(owner, run, operation, export_id)?;
        let mut private = self.extra_private_roots.clone();
        private.push(self.private_root.clone());
        let read = WorkspaceGrant::open(
            owner,
            &uuid::Uuid::new_v4().to_string(),
            &policy,
            root,
            &private,
        )?;
        if !read.has_persistent_identity()
            || read.resource_identity() != record.destination_resource
        {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let epoch = self.retention_scope(owner, &read)?;
        let grant = ExportInspectionGrant {
            read,
            run: run.into(),
            operation: operation.into(),
            export_id: export_id.into(),
            review_digest: outcome.review_digest,
            epoch,
        };
        self.validate_export_inspection(&grant)?;
        self.connection.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_inspection_consent')",params![run,operation]).map_err(JournalError::from)?;
        Ok(grant)
    }
    pub(super) fn validate_export_inspection(
        &self,
        grant: &ExportInspectionGrant,
    ) -> Result<(Record, RetentionExportOutcome), PatchError> {
        if self.retention_scope(grant.read.owner(), &grant.read)? != grant.epoch {
            return Err(WorkspaceError::Revoked.into());
        }
        let (record, outcome, policy) = self.export_inspection_record(
            grant.read.owner(),
            &grant.run,
            &grant.operation,
            &grant.export_id,
        )?;
        if grant.review_digest != outcome.review_digest
            || grant.read.policy() != policy
            || grant.read.resource_identity() != record.destination_resource
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        Ok((record, outcome))
    }
    pub async fn inspect_export_files(
        &self,
        grant: &ExportInspectionGrant,
    ) -> Result<ExportInspectionReceipt, PatchError> {
        let (record, outcome) = self.validate_export_inspection(grant)?;
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
            tokio::task::spawn_blocking(move || -> Result<ExportInspectionReceipt, PatchError> {
                let _slot = slot;
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let directory = &permit.inner.directory;
                let target = observe_export_file(directory, &record.file_name, &record.artifact)?;
                let staged = observe_export_file(directory, &record.temporary, &record.artifact)?;
                let receipt = ExportInspectionReceipt {
                    view: ExportInspectionView {
                        export_id: outcome.export_id,
                        review_digest: outcome.review_digest,
                        recorded_state: outcome.state,
                        file_name: record.file_name,
                        target,
                        staged,
                    },
                    permit,
                };
                receipt.view()?;
                Ok(receipt)
            });
        let result = tokio::time::timeout(grant.read.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        let receipt = result?;
        self.validate_export_inspection(grant)?;
        receipt.view()?;
        Ok(receipt)
    }
}
fn observe_export_file(
    directory: &Dir,
    name: &str,
    expected: &RetentionExportPreview,
) -> Result<ExportFileObservation, PatchError> {
    let named = match directory.symlink_metadata(name) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExportFileObservation::Missing);
        }
        Err(e) => return Err(io(e)),
    };
    if !named.is_file() || named.nlink() != 1 {
        return Ok(ExportFileObservation::Refused {
            reason: "not_single_link_regular_file".into(),
        });
    }
    if named.len() > 3 * MAX_TEXT as u64 {
        return Ok(ExportFileObservation::Refused {
            reason: "size_limit".into(),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = directory.open_with(name, &options).map_err(io)?;
    let before = authority::identity(directory, &file.metadata().map_err(io)?)?;
    if before != authority::identity(directory, &named)? {
        return Err(PatchError::VersionChanged);
    }
    let security = orange_platform_files::security_metadata(&file).map_err(io)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((3 * MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > 3 * MAX_TEXT
        || bytes.len() as u64 != named.len()
        || before != authority::identity(directory, &file.metadata().map_err(io)?)?
        || before != authority::identity(directory, &directory.symlink_metadata(name).map_err(io)?)?
        || security != orange_platform_files::security_metadata(&file).map_err(io)?
    {
        return Err(PatchError::VersionChanged);
    }
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    let size = bytes.len().to_string();
    let valid_export_document = verify_retention_export(&bytes).is_ok();
    let mut secured = before;
    secured.bind_security(security.digest());
    Ok(ExportFileObservation::Regular {
        matches_approved_artifact: sha256 == expected.artifact_sha256
            && size == expected.artifact_bytes,
        valid_export_document,
        bytes: size,
        sha256,
        security_sha256: hex::encode(security.digest()),
        object_digest: request_digest("export-inspected-object.v1", &secured)
            .map_err(|_| PatchError::Invalid)?,
    })
}
