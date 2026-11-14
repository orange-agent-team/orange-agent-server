use super::*;
mod inspection;
mod retirement;
mod writer;
pub use inspection::{
    ExportFileObservation, ExportInspectionGrant, ExportInspectionReceipt, ExportInspectionView,
};
pub use retirement::{
    ExportRetirementChoice, ExportRetirementGrant, ExportRetirementInspection,
    ExportRetirementInspectionReceipt, ExportRetirementOutcome, ExportRetirementReceipt,
    ExportRetirementReview, PreparedExportRetirement,
};
use std::io::{Seek, Write};
use std::time::Duration;
pub use writer::ExportQuiescence;

pub struct RetentionExportDestination {
    grant: WorkspaceGrant,
    operation: String,
    source_environment: String,
    epoch: i64,
}
impl RetentionExportDestination {
    pub fn request_revoke(&self) {
        self.grant.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.grant.wait_drained().await;
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionExportReview {
    pub export_id: String,
    pub digest: String,
    pub file_name: String,
    pub artifact: RetentionExportPreview,
    pub expires_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    owner: String,
    run: String,
    operation: String,
    source_environment: String,
    destination_environment: String,
    destination_resource: String,
    epoch: i64,
    expires_at: i64,
    temporary: String,
    file_name: String,
    artifact: RetentionExportPreview,
}
impl Record {
    fn digest(&self) -> Result<String, PatchError> {
        request_digest("retention-export-review.v1", self).map_err(|_| PatchError::Invalid)
    }
    fn review(&self, id: String) -> Result<RetentionExportReview, PatchError> {
        Ok(RetentionExportReview {
            export_id: id,
            digest: self.digest()?,
            file_name: self.file_name.clone(),
            artifact: self.artifact.clone(),
            expires_at: self.expires_at,
        })
    }
}
#[cfg(test)]
type PublicationHook = Box<dyn FnOnce() + Send>;
pub struct PreparedRetentionExport {
    review: RetentionExportReview,
    capture: RetentionExportCapture,
    destination: Permit,
    record: Record,
    #[cfg(test)]
    on_staged: std::sync::Mutex<Option<PublicationHook>>,
}
impl PreparedRetentionExport {
    #[cfg(test)]
    pub(crate) fn on_staged_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.on_staged.lock().expect("owned test hook") = Some(Box::new(hook));
    }

    pub fn preview(&self) -> Result<&RetentionExportReview, PatchError> {
        self.capture.check()?;
        if self.destination.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(&self.review)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionExportReceipt {
    pub export_id: String,
    pub review_digest: String,
    pub file_name: String,
    pub bytes: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionExportOutcome {
    pub export_id: String,
    pub review_digest: String,
    pub state: String,
}
struct OutcomeRow {
    body: String,
    digest: String,
    state: String,
    receipt: Option<String>,
    receipt_digest: Option<String>,
}
fn name_valid(name: &str) -> bool {
    name.len() <= 200
        && name.ends_with(".orange-retained.json")
        && components(name, false).is_ok_and(|parts| parts.len() == 1)
}
impl Journal {
    pub fn authorize_retention_export_destination(
        &self,
        source: &PatchRetentionGrant,
        root: &Path,
    ) -> Result<RetentionExportDestination, PatchError> {
        self.validate_retention_selection(source)?;
        let mut private = self.extra_private_roots.clone();
        private.push(self.private_root.clone());
        let grant = WorkspaceGrant::open(
            source.read.owner(),
            &uuid::Uuid::new_v4().to_string(),
            source.read.policy(),
            root,
            &private,
        )?;
        if !grant.has_persistent_identity() {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        Ok(RetentionExportDestination {
            grant,
            operation: source.operation.clone(),
            source_environment: source.read.environment().into(),
            epoch: source.epoch,
        })
    }
    fn validate_export_destination(
        &self,
        source: &PatchRetentionGrant,
        destination: &RetentionExportDestination,
    ) -> Result<(), PatchError> {
        self.validate_retention_selection(source)?;
        if destination.grant.is_revoked() || destination.epoch != source.epoch {
            return Err(WorkspaceError::Revoked.into());
        }
        if destination.operation != source.operation
            || destination.source_environment != source.read.environment()
            || destination.grant.owner() != source.read.owner()
            || destination.grant.policy() != source.read.policy()
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        Ok(())
    }
    pub async fn prepare_retention_export(
        &mut self,
        source: &PatchRetentionGrant,
        destination: &RetentionExportDestination,
        choice: RetentionExportChoice,
        file_name: &str,
        request_key: &str,
    ) -> Result<PreparedRetentionExport, PatchError> {
        if !valid_id(request_key) || !name_valid(file_name) {
            return Err(PatchError::Invalid);
        }
        self.validate_export_destination(source, destination)?;
        let capture = self.capture_retention_export(source, choice).await?;
        let target = destination
            .grant
            .permit(
                source.read.owner(),
                destination.grant.environment(),
                destination.grant.policy(),
            )
            .await?;
        match target.inner.directory.symlink_metadata(file_name) {
            Ok(_) => return Err(JournalError::Conflict.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        let prior:Option<(String,String,String,String)>=self.connection.query_row("SELECT export_id,body,digest,state FROM patch_exports WHERE owner=? AND request_key=?",params![source.read.owner(),request_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(JournalError::from)?;
        let (id, record) = if let Some((id, body, digest, state)) = prior {
            if body.len() > 32 * 1024 || state != "prepared" {
                return Err(JournalError::Conflict.into());
            }
            let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if record.digest()? != digest {
                return Err(PatchError::Invalid);
            }
            (id, record)
        } else {
            (
                uuid::Uuid::new_v4().to_string(),
                Record {
                    owner: source.read.owner().into(),
                    run: source.run.clone(),
                    operation: source.operation.clone(),
                    source_environment: source.read.environment().into(),
                    destination_environment: destination.grant.environment().into(),
                    destination_resource: destination.grant.resource_identity().into(),
                    epoch: source.epoch,
                    expires_at: now()?.checked_add(300).ok_or(PatchError::Invalid)?,
                    temporary: format!(".orange-agent-tmp-export-{}", uuid::Uuid::new_v4()),
                    file_name: file_name.into(),
                    artifact: capture.preview()?.clone(),
                },
            )
        };
        if record.owner != source.read.owner()
            || record.run != source.run
            || record.operation != source.operation
            || record.source_environment != source.read.environment()
            || record.destination_environment != destination.grant.environment()
            || record.destination_resource != destination.grant.resource_identity()
            || record.epoch != source.epoch
            || record.expires_at <= now()?
            || record.file_name != file_name
            || record.artifact != *capture.preview()?
        {
            return Err(JournalError::Conflict.into());
        }
        let temporary_id = record
            .temporary
            .strip_prefix(".orange-agent-tmp-export-")
            .ok_or(PatchError::Invalid)?;
        if uuid::Uuid::parse_str(temporary_id)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != temporary_id
        {
            return Err(PatchError::Invalid);
        }
        self.validate_export_destination(source, destination)?;
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        let digest = record.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM patch_exports WHERE owner=?",
                [source.read.owner()],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM patch_exports WHERE export_id=?)",
                [&id],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if !exists {
            if count >= 1000 {
                return Err(PatchError::Limit);
            }
            tx.execute("INSERT INTO patch_exports(export_id,operation_id,owner,request_key,host_epoch,expires_at,body,digest,state) VALUES(?,?,?,?,?,?,?,?,'prepared')",params![id,source.operation,source.read.owner(),request_key,source.epoch,record.expires_at,body,digest]).map_err(JournalError::from)?;
            tx.execute(
                "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_prepared')",
                params![source.run, source.operation],
            )
            .map_err(JournalError::from)?;
        }
        capture.check()?;
        if target.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(PreparedRetentionExport {
            review: record.review(id)?,
            capture,
            destination: target,
            record,
            #[cfg(test)]
            on_staged: std::sync::Mutex::new(None),
        })
    }
    pub async fn approve_retention_export(
        &mut self,
        source: &PatchRetentionGrant,
        destination: &RetentionExportDestination,
        prepared: PreparedRetentionExport,
        expected_digest: &str,
    ) -> Result<RetentionExportReceipt, PatchError> {
        self.validate_export_destination(source, destination)?;
        let review = prepared.preview()?.clone();
        let record = &prepared.record;
        if review.digest != expected_digest
            || record.digest()? != review.digest
            || record.owner != source.read.owner()
            || record.operation != source.operation
            || record.source_environment != source.read.environment()
            || record.destination_environment != destination.grant.environment()
            || record.destination_resource != destination.grant.resource_identity()
            || record.epoch != source.epoch
            || record.expires_at <= now()?
        {
            return Err(JournalError::ApprovalRequired.into());
        }
        let writer = {
            let writer_root = self.private_root.clone();
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(JournalError::from)?;
            let changed=tx.execute("UPDATE patch_exports SET state='dispatching' WHERE export_id=? AND owner=? AND state='prepared' AND digest=? AND body=? AND host_epoch=? AND expires_at>?",params![review.export_id,source.read.owner(),review.digest,serde_json::to_string(record).map_err(|_|PatchError::Invalid)?,source.epoch,now()?]).map_err(JournalError::from)?;
            if changed != 1 {
                return Err(JournalError::Conflict.into());
            }
            tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_dispatched')",
            params![source.run, source.operation],
        )
        .map_err(JournalError::from)?;
            let writer = writer::create(&writer_root, &review.export_id, &tx)?;
            prepared.preview()?;
            tx.commit().map_err(JournalError::from)?;
            writer
        };
        let mut cancellation = CancellationGuard {
            inner: prepared.destination.inner.clone(),
            finished: false,
        };
        let work = tokio::task::spawn_blocking(move || publish(prepared, writer));
        let result = tokio::time::timeout(Duration::from_secs(30), work).await;
        let result = match result {
            Ok(Ok(result)) => {
                cancellation.finished = true;
                result
            }
            _ => Err(PatchError::RecoveryRequired),
        };
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(error) => {
                let _=self.connection.execute("UPDATE patch_exports SET state='unknown' WHERE export_id=? AND state='dispatching'",[&review.export_id]);
                return Err(error);
            }
        };
        if self
            .validate_export_destination(source, destination)
            .is_err()
        {
            let _=self.connection.execute("UPDATE patch_exports SET state='unknown' WHERE export_id=? AND state='dispatching'",[&review.export_id]);
            return Err(PatchError::RecoveryRequired);
        }
        let commit = (|| -> Result<(), PatchError> {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(JournalError::from)?;
            let body = serde_json::to_string(&receipt).map_err(|_| PatchError::Invalid)?;
            let digest = request_digest("retention-export-receipt.v1", &receipt)
                .map_err(|_| PatchError::Invalid)?;
            if tx.execute("UPDATE patch_exports SET state='succeeded',receipt=?,receipt_digest=? WHERE export_id=? AND state='dispatching'",params![body,digest,review.export_id]).map_err(JournalError::from)?!=1 {return Err(PatchError::RecoveryRequired);}
            tx.execute(
                "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_completed')",
                params![source.run, source.operation],
            )
            .map_err(JournalError::from)?;
            tx.commit().map_err(JournalError::from)?;
            Ok(())
        })();
        if commit.is_err() {
            let _=self.connection.execute("UPDATE patch_exports SET state='unknown' WHERE export_id=? AND state='dispatching'",[&review.export_id]);
            return Err(PatchError::RecoveryRequired);
        }
        Ok(receipt)
    }
    /// Bounded metadata discovery after restart. No paths, contents or restored
    /// filesystem capabilities. Each returned outcome is independently verified.
    pub fn retention_export_outcomes(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        before: Option<&str>,
        limit: u16,
    ) -> Result<Vec<RetentionExportOutcome>, PatchError> {
        if ![owner, run, operation].iter().all(|id| valid_id(id))
            || before.is_some_and(|id| !valid_id(id))
            || !(1..=100).contains(&limit)
        {
            return Err(PatchError::Invalid);
        }
        let owned: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations o JOIN runtime_runs r ON r.run_id=o.run_id JOIN editing_publications p USING(operation_id) WHERE o.owner=? AND o.run_id=? AND o.operation_id=?)",
            params![owner, run, operation], |r| r.get(0),
        ).map_err(JournalError::from)?;
        if !owned {
            return Err(WorkspaceError::Forbidden.into());
        }
        let mut statement = self.connection.prepare(
            "SELECT export_id FROM patch_exports WHERE owner=?1 AND operation_id=?2 AND (?3 IS NULL OR export_id<?3) ORDER BY export_id DESC LIMIT ?4",
        ).map_err(JournalError::from)?;
        let ids = statement
            .query_map(params![owner, operation, before, i64::from(limit)], |r| {
                r.get::<_, String>(0)
            })
            .map_err(JournalError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(JournalError::from)?;
        ids.iter()
            .map(|id| {
                self.retention_export_outcome(owner, run, operation, id)?
                    .ok_or(PatchError::Invalid)
            })
            .collect()
    }
    pub fn retention_export_outcome(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        export_id: &str,
    ) -> Result<Option<RetentionExportOutcome>, PatchError> {
        if ![owner, run, operation, export_id]
            .iter()
            .all(|id| valid_id(id))
        {
            return Err(PatchError::Invalid);
        }
        let row: Option<OutcomeRow> = self.connection.query_row(
            "SELECT e.body,e.digest,e.state,e.receipt,e.receipt_digest FROM patch_exports e JOIN operations o ON o.operation_id=e.operation_id WHERE e.export_id=? AND e.owner=? AND o.owner=? AND o.run_id=? AND o.operation_id=?",
            params![export_id,owner,owner,run,operation], |r| Ok(OutcomeRow {body:r.get(0)?,digest:r.get(1)?,state:r.get(2)?,receipt:r.get(3)?,receipt_digest:r.get(4)?}),
        ).optional().map_err(JournalError::from)?;
        let Some(OutcomeRow {
            body,
            digest,
            state,
            receipt,
            receipt_digest,
        }) = row
        else {
            return Ok(None);
        };
        if body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Record = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        if record.owner != owner
            || record.run != run
            || record.operation != operation
            || record.digest()? != digest
        {
            return Err(PatchError::Invalid);
        }
        if state == "succeeded" {
            let encoded = receipt.ok_or(PatchError::Invalid)?;
            if encoded.len() > 8192 {
                return Err(PatchError::Limit);
            }
            let r: RetentionExportReceipt =
                serde_json::from_str(&encoded).map_err(|_| PatchError::Invalid)?;
            if r.export_id != export_id
                || r.review_digest != digest
                || r.file_name != record.file_name
                || r.bytes != record.artifact.artifact_bytes
                || r.sha256 != record.artifact.artifact_sha256
                || Some(
                    request_digest("retention-export-receipt.v1", &r)
                        .map_err(|_| PatchError::Invalid)?,
                ) != receipt_digest
            {
                return Err(PatchError::Invalid);
            }
        } else if receipt.is_some() || receipt_digest.is_some() {
            return Err(PatchError::Invalid);
        }
        Ok(Some(RetentionExportOutcome {
            export_id: export_id.into(),
            review_digest: digest,
            state,
        }))
    }
}
fn same_file(directory: &Dir, name: &str, file: &cap_std::fs::File) -> Result<bool, PatchError> {
    let named = directory.symlink_metadata(name).map_err(io)?;
    let open = file.metadata().map_err(io)?;
    Ok(named.is_file()
        && named.nlink() == 1
        && named.dev() == open.dev()
        && named.ino() == open.ino())
}
fn publish(
    prepared: PreparedRetentionExport,
    writer: writer::WriterLease,
) -> Result<RetentionExportReceipt, PatchError> {
    let writer = writer;
    writer.check()?;
    prepared.preview()?;
    let directory = &prepared.destination.inner.directory;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    use cap_std::fs::OpenOptionsExt;
    options.mode(0o600);
    let mut file = directory
        .open_with(&prepared.record.temporary, &options)
        .map_err(io)?;
    for chunk in prepared.capture.encoded_bytes()?.chunks(64 * 1024) {
        if prepared.destination.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        prepared.capture.check()?;
        file.write_all(chunk).map_err(io)?;
    }
    file.sync_all().map_err(io)?;
    let verify = |file: &mut cap_std::fs::File| -> Result<(), PatchError> {
        file.rewind().map_err(io)?;
        let mut bytes = Vec::new();
        file.take((3 * MAX_TEXT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes != prepared.capture.encoded_bytes()? {
            return Err(PatchError::VersionChanged);
        }
        Ok(())
    };
    verify(&mut file)?;
    #[cfg(test)]
    if let Some(hook) = prepared.on_staged.lock().expect("owned test hook").take() {
        hook();
    }
    prepared.preview()?;
    if !same_file(directory, &prepared.record.temporary, &file)? {
        return Err(PatchError::VersionChanged);
    }
    writer.check()?;
    rustix::fs::renameat_with(
        directory,
        &prepared.record.temporary,
        directory,
        &prepared.record.file_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|_| PatchError::RecoveryRequired)?;
    if !same_file(directory, &prepared.record.file_name, &file)? {
        return Err(PatchError::RecoveryRequired);
    }
    verify(&mut file)?;
    directory
        .try_clone()
        .map_err(io)?
        .into_std_file()
        .sync_all()
        .map_err(io)?;
    prepared.preview()?;
    writer.check()?;
    Ok(RetentionExportReceipt {
        export_id: prepared.review.export_id.clone(),
        review_digest: prepared.review.digest.clone(),
        file_name: prepared.record.file_name.clone(),
        bytes: prepared.review.artifact.artifact_bytes.clone(),
        sha256: prepared.review.artifact.artifact_sha256.clone(),
    })
}
