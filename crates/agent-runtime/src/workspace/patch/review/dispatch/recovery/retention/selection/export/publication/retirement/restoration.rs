//! Explicit recovery copy from a bound capsule. Original capsule always remains.
use super::*;

pub struct ExportRestorationGrant(ExportRetirementGrant);
impl ExportRestorationGrant {
    pub fn request_revoke(&self) {
        self.0.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.0.wait_drained().await;
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Restoration {
    owner: String,
    run: String,
    operation: String,
    export_id: String,
    retirement_id: String,
    retirement_digest: String,
    read_environment: String,
    write_environment: String,
    resource: String,
    epoch: i64,
    expires_at: i64,
    file_name: String,
    temporary: String,
    source: ExportFileObservation,
}
impl Restoration {
    fn digest(&self) -> Result<String, PatchError> {
        request_digest("export-restoration-review.v1", self).map_err(|_| PatchError::Invalid)
    }
    fn review(&self, id: String) -> Result<ExportRestorationReview, PatchError> {
        Ok(ExportRestorationReview {
            restoration_id: id,
            retirement_id: self.retirement_id.clone(),
            export_id: self.export_id.clone(),
            digest: self.digest()?,
            file_name: self.file_name.clone(),
            source: self.source.clone(),
            expires_at: self.expires_at,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRestorationReview {
    pub restoration_id: String,
    pub retirement_id: String,
    pub export_id: String,
    pub digest: String,
    pub file_name: String,
    pub source: ExportFileObservation,
    pub expires_at: i64,
}
pub struct PreparedExportRestoration {
    record: Restoration,
    review: ExportRestorationReview,
    quiescence: ExportQuiescence,
    write: Permit,
    #[cfg(test)]
    before_publish: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl PreparedExportRestoration {
    pub fn preview(&self) -> Result<&ExportRestorationReview, PatchError> {
        self.live()?;
        Ok(&self.review)
    }
    fn live(&self) -> Result<(), PatchError> {
        self.quiescence.check()?;
        if self.write.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn before_publish_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_publish.lock().expect("hook") = Some(Box::new(hook));
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRestorationOutcome {
    pub restoration_id: String,
    pub retirement_id: String,
    pub export_id: String,
    pub review_digest: String,
    pub file_name: String,
    pub state: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    restoration_id: String,
    review_digest: String,
    bytes: String,
    sha256: String,
    disposition: String,
}

impl Journal {
    /// Independent write selection for recovery copy, not a retirement grant reuse.
    pub fn authorize_export_restoration(
        &self,
        read: &ExportInspectionGrant,
        root: &Path,
    ) -> Result<ExportRestorationGrant, PatchError> {
        Ok(ExportRestorationGrant(
            self.authorize_export_retirement(read, root)?,
        ))
    }
    fn restoration_capsule(
        &self,
        read: &ExportInspectionGrant,
        id: &str,
    ) -> Result<(Record, ExportRetirementOutcome, Retirement, Capsule), PatchError> {
        let (export, parent) = self.validate_export_inspection(read)?;
        let outcome = self
            .export_retirement_outcome(&export.owner, &export.run, &export.operation, id)?
            .ok_or(WorkspaceError::Forbidden)?;
        if outcome.export_id != parent.export_id
            || !matches!(outcome.state.as_str(), "retained" | "unknown")
        {
            return Err(PatchError::RecoveryRequired);
        }
        let (body,binding,digest):(String,String,String)=self.connection.query_row("SELECT r.body,c.body,c.digest FROM export_retirements r JOIN export_retirement_capsules c USING(retirement_id) WHERE r.retirement_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(JournalError::from)?.ok_or(PatchError::RecoveryRequired)?;
        if body.len() > 32768 || binding.len() > 4096 {
            return Err(PatchError::Limit);
        }
        let retirement: Retirement =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let capsule: Capsule = serde_json::from_str(&binding).map_err(|_| PatchError::Invalid)?;
        if request_digest("export-retirement-capsule.v1", &capsule)
            .map_err(|_| PatchError::Invalid)?
            != digest
        {
            return Err(PatchError::Invalid);
        }
        Ok((export, outcome, retirement, capsule))
    }
    pub async fn prepare_export_restoration(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRestorationGrant,
        retirement_id: &str,
        file_name: &str,
        key: &str,
    ) -> Result<PreparedExportRestoration, PatchError> {
        if !valid_id(key) || !name_valid(file_name) {
            return Err(PatchError::Invalid);
        }
        self.validate_retirement(read, &write.0)?;
        let quiescence = self.acquire_export_quiescence(read).await?;
        let (export, parent, retirement, binding) =
            self.restoration_capsule(read, retirement_id)?;
        let inspection = self.inspect_export_retirement(read, retirement_id).await?;
        let source = inspection
            .view()?
            .retained
            .clone()
            .ok_or(PatchError::RecoveryRequired)?;
        if !matches!(
            source,
            ExportFileObservation::Regular {
                matches_approved_artifact: true,
                valid_export_document: true,
                ..
            }
        ) {
            return Err(PatchError::RecoveryRequired);
        }
        let permit = write
            .0
            .write
            .permit(
                read.read.owner(),
                write.0.write.environment(),
                read.read.policy(),
            )
            .await?;
        let capsule = open_directory(&permit.inner.directory, &[&retirement.capsule])?;
        check_capsule(
            &permit.inner.directory,
            &capsule,
            &retirement.capsule,
            &retirement.nonce,
            &binding,
        )?;
        if observe_export_file(&permit.inner.directory, file_name, &export.artifact)?
            != ExportFileObservation::Missing
        {
            return Err(PatchError::VersionChanged);
        }
        let old:Option<(String,String,String,String)>=self.connection.query_row("SELECT restoration_id,body,digest,state FROM export_restorations WHERE owner=? AND request_key=?",params![export.owner,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(JournalError::from)?;
        let (id, record) = if let Some((id, body, digest, state)) = old {
            if body.len() > 32768 || state != "prepared" {
                return Err(JournalError::Conflict.into());
            }
            let value: Restoration =
                serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if value.digest()? != digest {
                return Err(PatchError::Invalid);
            }
            (id, value)
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            let temporary = format!(".orange-agent-tmp-export-restore-{id}");
            (
                id,
                Restoration {
                    owner: export.owner.clone(),
                    run: export.run.clone(),
                    operation: export.operation.clone(),
                    export_id: parent.export_id.clone(),
                    retirement_id: retirement_id.into(),
                    retirement_digest: parent.review_digest.clone(),
                    read_environment: read.read.environment().into(),
                    write_environment: write.0.write.environment().into(),
                    resource: export.destination_resource.clone(),
                    epoch: write.0.epoch,
                    expires_at: now()?.checked_add(300).ok_or(PatchError::Invalid)?,
                    file_name: file_name.into(),
                    temporary,
                    source: source.clone(),
                },
            )
        };
        if uuid::Uuid::parse_str(&id)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != id
            || record.temporary != format!(".orange-agent-tmp-export-restore-{id}")
        {
            return Err(PatchError::Invalid);
        }
        if record.owner != export.owner
            || record.run != export.run
            || record.operation != export.operation
            || record.export_id != parent.export_id
            || record.retirement_id != retirement_id
            || record.retirement_digest != parent.review_digest
            || record.read_environment != read.read.environment()
            || record.write_environment != write.0.write.environment()
            || record.resource != export.destination_resource
            || record.epoch != write.0.epoch
            || record.expires_at <= now()?
            || record.file_name != file_name
            || record.source != source
        {
            return Err(JournalError::Conflict.into());
        }
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        let digest = record.digest()?;
        self.validate_retirement(read, &write.0)?;
        quiescence.check()?;
        if permit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM export_restorations WHERE restoration_id=?)",
                [&id],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if !exists {
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM export_restorations WHERE owner=?",
                    [&export.owner],
                    |r| r.get(0),
                )
                .map_err(JournalError::from)?;
            if count >= 1000 {
                return Err(PatchError::Limit);
            }
            tx.execute("INSERT INTO export_restorations(restoration_id,retirement_id,owner,request_key,host_epoch,body,digest,state) VALUES(?,?,?,?,?,?,?,'prepared')",params![id,retirement_id,export.owner,key,record.epoch,body,digest]).map_err(JournalError::from)?;
            tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_restoration_prepared')",params![export.run,export.operation]).map_err(JournalError::from)?;
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(PreparedExportRestoration {
            review: record.review(id)?,
            record,
            quiescence,
            write: permit,
            #[cfg(test)]
            before_publish: std::sync::Mutex::new(None),
        })
    }
    pub async fn approve_export_restoration(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRestorationGrant,
        prepared: PreparedExportRestoration,
        expected: &str,
    ) -> Result<ExportRestorationOutcome, PatchError> {
        if !matches!(
            tokio::runtime::Handle::current().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            return Err(PatchError::RecoveryRequired);
        }
        self.validate_retirement(read, &write.0)?;
        prepared.live()?;
        let record = &prepared.record;
        if record.digest()? != expected
            || prepared.review.digest != expected
            || record.read_environment != read.read.environment()
            || record.write_environment != write.0.write.environment()
            || record.export_id != write.0.export_id
            || record.owner != read.read.owner()
            || record.epoch != write.0.epoch
            || record.expires_at <= now()?
        {
            return Err(JournalError::ApprovalRequired.into());
        }
        let (export, parent, retirement, binding) =
            self.restoration_capsule(read, &record.retirement_id)?;
        if record.retirement_digest != parent.review_digest {
            return Err(PatchError::Invalid);
        }
        let capsule = open_directory(&prepared.write.inner.directory, &[&retirement.capsule])?;
        let bytes = read_capsule(&prepared, &export, &retirement, &binding, &capsule)?;
        if observe_export_file(
            &prepared.write.inner.directory,
            &record.file_name,
            &export.artifact,
        )? != ExportFileObservation::Missing
        {
            return Err(PatchError::VersionChanged);
        }
        let id = prepared.review.restoration_id.clone();
        {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(JournalError::from)?;
            if tx.execute("UPDATE export_restorations SET state='dispatching' WHERE restoration_id=? AND body=? AND digest=? AND host_epoch=? AND state='prepared'",params![id,serde_json::to_string(record).map_err(|_|PatchError::Invalid)?,expected,record.epoch]).map_err(JournalError::from)?!=1{return Err(JournalError::Conflict.into());}
            tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_restoration_dispatched')",params![record.run,record.operation]).map_err(JournalError::from)?;
            prepared.live()?;
            if record.expires_at <= now()? {
                return Err(JournalError::ApprovalRequired.into());
            }
            tx.commit().map_err(JournalError::from)?;
        }
        let result = tokio::task::block_in_place(|| {
            self.publish_export_restoration(
                read,
                write,
                prepared,
                CopySource {
                    export: &export,
                    retirement: &retirement,
                    binding: &binding,
                    capsule: &capsule,
                    bytes: &bytes,
                },
            )
        });
        if result.is_err() {
            let _=self.connection.execute("UPDATE export_restorations SET state='unknown' WHERE restoration_id=? AND state='dispatching'",[&id]);
        }
        result
    }
    fn publish_export_restoration(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRestorationGrant,
        prepared: PreparedExportRestoration,
        source: CopySource<'_>,
    ) -> Result<ExportRestorationOutcome, PatchError> {
        let CopySource {
            export,
            retirement,
            binding,
            capsule,
            bytes,
        } = source;
        prepared.live()?;
        self.validate_retirement(read, &write.0)?;
        let record = &prepared.record;
        let parent = &prepared.write.inner.directory;
        check_capsule(
            parent,
            capsule,
            &retirement.capsule,
            &retirement.nonce,
            binding,
        )?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .read(true)
            .create_new(true)
            .follow(FollowSymlinks::No)
            .mode(0o600);
        let mut staged = parent.open_with(&record.temporary, &options).map_err(io)?;
        staged.write_all(bytes).map_err(io)?;
        staged.sync_all().map_err(io)?;
        sync(parent)?;
        #[cfg(test)]
        if let Some(hook) = prepared.before_publish.lock().expect("hook").take() {
            hook();
        }
        prepared.live()?;
        self.validate_retirement(read, &write.0)?;
        if record.expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        if !same_file(parent, &record.temporary, &staged)?
            || !matches!(
                observe_export_file(parent, &record.temporary, &export.artifact)?,
                ExportFileObservation::Regular {
                    matches_approved_artifact: true,
                    valid_export_document: true,
                    ..
                }
            )
        {
            return Err(PatchError::RecoveryRequired);
        }
        if read_capsule(&prepared, export, retirement, binding, capsule)? != bytes {
            return Err(PatchError::VersionChanged);
        }
        rustix::fs::renameat_with(
            parent,
            &record.temporary,
            parent,
            &record.file_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|e| io(e.into()))?;
        sync(parent)?;
        if !same_file(parent, &record.file_name, &staged)?
            || !matches!(
                observe_export_file(parent, &record.file_name, &export.artifact)?,
                ExportFileObservation::Regular {
                    matches_approved_artifact: true,
                    valid_export_document: true,
                    ..
                }
            )
        {
            return Err(PatchError::RecoveryRequired);
        }
        prepared.live()?;
        check_capsule(
            parent,
            capsule,
            &retirement.capsule,
            &retirement.nonce,
            binding,
        )?;
        let receipt = Receipt {
            restoration_id: prepared.review.restoration_id.clone(),
            review_digest: prepared.review.digest.clone(),
            bytes: bytes.len().to_string(),
            sha256: hex::encode(sha2::Sha256::digest(bytes)),
            disposition: "copy_restored_capsule_retained".into(),
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        if tx.execute("UPDATE export_restorations SET state='succeeded',receipt=?,receipt_digest=? WHERE restoration_id=? AND state='dispatching'",params![serde_json::to_string(&receipt).map_err(|_|PatchError::Invalid)?,request_digest("export-restoration-receipt.v1",&receipt).map_err(|_|PatchError::Invalid)?,receipt.restoration_id]).map_err(JournalError::from)?!=1{return Err(PatchError::RecoveryRequired);}
        tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_restoration_completed')",params![record.run,record.operation]).map_err(JournalError::from)?;
        prepared.live()?;
        tx.commit().map_err(JournalError::from)?;
        self.export_restoration_outcome(
            &record.owner,
            &record.run,
            &record.operation,
            &receipt.restoration_id,
        )?
        .ok_or(PatchError::RecoveryRequired)
    }
}
struct CopySource<'a> {
    export: &'a Record,
    retirement: &'a Retirement,
    binding: &'a Capsule,
    capsule: &'a Dir,
    bytes: &'a [u8],
}
fn read_capsule(
    prepared: &PreparedExportRestoration,
    export: &Record,
    retirement: &Retirement,
    binding: &Capsule,
    capsule: &Dir,
) -> Result<Vec<u8>, PatchError> {
    prepared.live()?;
    check_capsule(
        &prepared.write.inner.directory,
        capsule,
        &retirement.capsule,
        &retirement.nonce,
        binding,
    )?;
    if observe_export_file(capsule, "item", &export.artifact)? != prepared.record.source {
        return Err(PatchError::VersionChanged);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = capsule.open_with("item", &options).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > 3 * 1024 * 1024 {
        return Err(PatchError::RecoveryRequired);
    }
    let mut bytes = Vec::new();
    file.take(3 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if !matches!(&prepared.record.source,ExportFileObservation::Regular {bytes:b,sha256,..} if b==&bytes.len().to_string()&&sha256==&hex::encode(sha2::Sha256::digest(&bytes)))
    {
        return Err(PatchError::VersionChanged);
    }
    if observe_export_file(capsule, "item", &export.artifact)? != prepared.record.source {
        return Err(PatchError::VersionChanged);
    }
    check_capsule(
        &prepared.write.inner.directory,
        capsule,
        &retirement.capsule,
        &retirement.nonce,
        binding,
    )?;
    prepared.live()?;
    Ok(bytes)
}
impl Journal {
    fn restoration_record(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        id: &str,
    ) -> Result<Option<(Restoration, ExportRestorationOutcome)>, PatchError> {
        if ![owner, run, operation, id].iter().all(|v| valid_id(v)) {
            return Err(PatchError::Invalid);
        }
        type Row = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        );
        let row:Option<Row>=self.connection.query_row("SELECT s.body,s.digest,s.state,s.receipt,s.receipt_digest,s.retirement_id FROM export_restorations s JOIN export_retirements r USING(retirement_id) JOIN patch_exports e ON e.export_id=r.export_id JOIN operations o ON o.operation_id=e.operation_id WHERE s.restoration_id=? AND s.owner=? AND r.owner=? AND e.owner=? AND o.owner=? AND o.run_id=? AND o.operation_id=?",params![id,owner,owner,owner,owner,run,operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(JournalError::from)?;
        let Some((body, digest, state, receipt, receipt_digest, parent_id)) = row else {
            return Ok(None);
        };
        if body.len() > 32768 {
            return Err(PatchError::Limit);
        }
        let record: Restoration = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let parent = self
            .export_retirement_outcome(owner, run, operation, &parent_id)?
            .ok_or(PatchError::Invalid)?;
        if uuid::Uuid::parse_str(id)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != id
            || record.owner != owner
            || record.run != run
            || record.operation != operation
            || record.export_id != parent.export_id
            || record.retirement_id != parent_id
            || record.retirement_digest != parent.review_digest
            || record.digest()? != digest
            || !name_valid(&record.file_name)
            || record.temporary != format!(".orange-agent-tmp-export-restore-{id}")
            || !matches!(
                state.as_str(),
                "prepared" | "dispatching" | "succeeded" | "unknown"
            )
        {
            return Err(PatchError::Invalid);
        }
        if state == "succeeded" {
            let body = receipt.ok_or(PatchError::Invalid)?;
            if body.len() > 8192 {
                return Err(PatchError::Limit);
            }
            let value: Receipt = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if value.restoration_id != id
                || value.review_digest != digest
                || value.disposition != "copy_restored_capsule_retained"
                || !matches!(&record.source,ExportFileObservation::Regular {bytes,sha256,..} if bytes==&value.bytes&&sha256==&value.sha256)
                || Some(
                    request_digest("export-restoration-receipt.v1", &value)
                        .map_err(|_| PatchError::Invalid)?,
                ) != receipt_digest
            {
                return Err(PatchError::Invalid);
            }
        } else if receipt.is_some() || receipt_digest.is_some() {
            return Err(PatchError::Invalid);
        }
        let outcome = ExportRestorationOutcome {
            restoration_id: id.into(),
            retirement_id: record.retirement_id.clone(),
            export_id: record.export_id.clone(),
            review_digest: digest,
            file_name: record.file_name.clone(),
            state,
        };
        Ok(Some((record, outcome)))
    }
    pub fn export_restoration_outcome(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        id: &str,
    ) -> Result<Option<ExportRestorationOutcome>, PatchError> {
        Ok(self
            .restoration_record(owner, run, operation, id)?
            .map(|(_, v)| v))
    }
    pub fn export_restoration_outcomes(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        retirement_id: &str,
        before: Option<&str>,
        limit: u16,
    ) -> Result<Vec<ExportRestorationOutcome>, PatchError> {
        if before.is_some_and(|v| !valid_id(v)) || !(1..=100).contains(&limit) {
            return Err(PatchError::Invalid);
        }
        self.export_retirement_outcome(owner, run, operation, retirement_id)?
            .ok_or(WorkspaceError::Forbidden)?;
        let mut statement=self.connection.prepare("SELECT restoration_id FROM export_restorations WHERE owner=?1 AND retirement_id=?2 AND (?3 IS NULL OR restoration_id<?3) ORDER BY restoration_id DESC LIMIT ?4").map_err(JournalError::from)?;
        let ids = statement
            .query_map(
                params![owner, retirement_id, before, i64::from(limit)],
                |r| r.get::<_, String>(0),
            )
            .map_err(JournalError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(JournalError::from)?;
        ids.iter()
            .map(|id| {
                self.export_restoration_outcome(owner, run, operation, id)?
                    .ok_or(PatchError::Invalid)
            })
            .collect()
    }
    pub async fn inspect_export_restoration(
        &self,
        read: &ExportInspectionGrant,
        id: &str,
    ) -> Result<ExportRestorationInspectionReceipt, PatchError> {
        let (export, parent) = self.validate_export_inspection(read)?;
        let (record, outcome) = self
            .restoration_record(&export.owner, &export.run, &export.operation, id)?
            .ok_or(WorkspaceError::Forbidden)?;
        if outcome.export_id != parent.export_id {
            return Err(WorkspaceError::Forbidden.into());
        }
        let slot =
            tokio::time::timeout(read.read.inner.deadline, READ_SLOTS.clone().acquire_owned())
                .await
                .map_err(|_| WorkspaceError::Timeout)?
                .map_err(|_| WorkspaceError::Interrupted)?;
        let permit = read
            .read
            .permit(
                read.read.owner(),
                read.read.environment(),
                read.read.policy(),
            )
            .await?;
        let mut cancellation = CancellationGuard {
            inner: permit.inner.clone(),
            finished: false,
        };
        let work = tokio::task::spawn_blocking(
            move || -> Result<ExportRestorationInspectionReceipt, PatchError> {
                let _slot = slot;
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let target = observe_export_file(
                    &permit.inner.directory,
                    &record.file_name,
                    &export.artifact,
                )?;
                let staged = observe_export_file(
                    &permit.inner.directory,
                    &record.temporary,
                    &export.artifact,
                )?;
                let receipt = ExportRestorationInspectionReceipt {
                    view: ExportRestorationInspection {
                        restoration_id: outcome.restoration_id,
                        retirement_id: outcome.retirement_id,
                        export_id: outcome.export_id,
                        recorded_state: outcome.state,
                        file_name: record.file_name,
                        target,
                        staged,
                    },
                    read: permit,
                };
                receipt.view()?;
                Ok(receipt)
            },
        );
        let result = tokio::time::timeout(read.read.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        let receipt = result?;
        self.validate_export_inspection(read)?;
        receipt.view()?;
        Ok(receipt)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRestorationInspection {
    pub restoration_id: String,
    pub retirement_id: String,
    pub export_id: String,
    pub recorded_state: String,
    pub file_name: String,
    pub target: ExportFileObservation,
    pub staged: ExportFileObservation,
}
pub struct ExportRestorationInspectionReceipt {
    view: ExportRestorationInspection,
    read: Permit,
}
impl ExportRestorationInspectionReceipt {
    pub fn view(&self) -> Result<&ExportRestorationInspection, PatchError> {
        if self.read.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(&self.view)
    }
}
