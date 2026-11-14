mod restoration;
pub use restoration::{
    ExportRestorationGrant, ExportRestorationInspection, ExportRestorationInspectionReceipt,
    ExportRestorationOutcome, ExportRestorationReview, PreparedExportRestoration,
};
// Explicit reversible removal; captured objects remain retained.
use super::*;
use cap_std::fs::OpenOptionsExt;
use inspection::observe_export_file;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportRetirementChoice {
    Target,
    Staged,
}
pub struct ExportRetirementGrant {
    write: WorkspaceGrant,
    export_id: String,
    reader_environment: String,
    epoch: i64,
}
impl ExportRetirementGrant {
    pub fn request_revoke(&self) {
        self.write.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.write.wait_drained().await;
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRetirementReview {
    pub retirement_id: String,
    pub export_id: String,
    pub digest: String,
    pub choice: ExportRetirementChoice,
    pub file_name: String,
    pub observed: ExportFileObservation,
    pub expires_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retirement {
    owner: String,
    run: String,
    operation: String,
    export_id: String,
    export_digest: String,
    read_environment: String,
    write_environment: String,
    resource: String,
    epoch: i64,
    expires_at: i64,
    choice: ExportRetirementChoice,
    file_name: String,
    capsule: String,
    nonce: String,
    observed: ExportFileObservation,
}
impl Retirement {
    fn digest(&self) -> Result<String, PatchError> {
        request_digest("export-retirement-review.v1", self).map_err(|_| PatchError::Invalid)
    }
    fn review(&self, id: String) -> Result<ExportRetirementReview, PatchError> {
        Ok(ExportRetirementReview {
            retirement_id: id,
            export_id: self.export_id.clone(),
            digest: self.digest()?,
            choice: self.choice,
            file_name: self.file_name.clone(),
            observed: self.observed.clone(),
            expires_at: self.expires_at,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capsule {
    device: String,
    inode: String,
    nonce_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRetirementReceipt {
    pub retirement_id: String,
    pub export_id: String,
    pub review_digest: String,
    pub choice: ExportRetirementChoice,
    pub bytes: String,
    pub sha256: String,
    pub disposition: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRetirementOutcome {
    pub retirement_id: String,
    pub export_id: String,
    pub review_digest: String,
    pub state: String,
}
pub struct PreparedExportRetirement {
    review: ExportRetirementReview,
    record: Retirement,
    quiescence: ExportQuiescence,
    write: Permit,
    #[cfg(test)]
    before_capture: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl PreparedExportRetirement {
    pub fn preview(&self) -> Result<&ExportRetirementReview, PatchError> {
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
    pub(crate) fn before_capture_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_capture.lock().expect("test hook") = Some(Box::new(hook));
    }
}
impl Journal {
    pub fn authorize_export_retirement(
        &self,
        read: &ExportInspectionGrant,
        root: &Path,
    ) -> Result<ExportRetirementGrant, PatchError> {
        let (record, outcome) = self.validate_export_inspection(read)?;
        let mut private = self.extra_private_roots.clone();
        private.push(self.private_root.clone());
        let write = WorkspaceGrant::open(
            read.read.owner(),
            &uuid::Uuid::new_v4().to_string(),
            read.read.policy(),
            root,
            &private,
        )?;
        if !write.has_persistent_identity()
            || write.resource_identity() != record.destination_resource
        {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        Ok(ExportRetirementGrant {
            epoch: self.retention_scope(read.read.owner(), &read.read)?,
            write,
            export_id: outcome.export_id,
            reader_environment: read.read.environment().into(),
        })
    }
    fn validate_retirement(
        &self,
        read: &ExportInspectionGrant,
        write: &ExportRetirementGrant,
    ) -> Result<(Record, RetentionExportOutcome), PatchError> {
        let (record, outcome) = self.validate_export_inspection(read)?;
        if write.write.is_revoked()
            || write.epoch != self.retention_scope(read.read.owner(), &read.read)?
        {
            return Err(WorkspaceError::Revoked.into());
        }
        if write.export_id != outcome.export_id
            || write.reader_environment != read.read.environment()
            || write.write.owner() != read.read.owner()
            || write.write.policy() != read.read.policy()
            || write.write.resource_identity() != record.destination_resource
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        Ok((record, outcome))
    }
    pub async fn prepare_export_retirement(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRetirementGrant,
        choice: ExportRetirementChoice,
        key: &str,
    ) -> Result<PreparedExportRetirement, PatchError> {
        if !valid_id(key) {
            return Err(PatchError::Invalid);
        }
        let (export, outcome) = self.validate_retirement(read, write)?;
        let quiescence = self.acquire_export_quiescence(read).await?;
        let snapshot = self.inspect_export_files(read).await?;
        let view = snapshot.view()?;
        let observed = match choice {
            ExportRetirementChoice::Target => view.target.clone(),
            ExportRetirementChoice::Staged => view.staged.clone(),
        };
        // A conflicting or incomplete object is never silently treated as ours.
        if !matches!(
            observed,
            ExportFileObservation::Regular {
                matches_approved_artifact: true,
                valid_export_document: true,
                ..
            }
        ) {
            return Err(PatchError::RecoveryRequired);
        }
        let permit = write
            .write
            .permit(
                read.read.owner(),
                write.write.environment(),
                write.write.policy(),
            )
            .await?;
        let prior:Option<(String,String,String,String)>=self.connection.query_row("SELECT retirement_id,body,digest,state FROM export_retirements WHERE owner=? AND request_key=?",params![export.owner,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(JournalError::from)?;
        let (id, record) = if let Some((id, body, digest, state)) = prior {
            if body.len() > 32 * 1024 || state != "prepared" {
                return Err(JournalError::Conflict.into());
            }
            let record: Retirement =
                serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if record.digest()? != digest {
                return Err(PatchError::Invalid);
            }
            (id, record)
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            let capsule = format!(".orange-agent-tmp-retired-{id}");
            (
                id,
                Retirement {
                    owner: export.owner.clone(),
                    run: export.run.clone(),
                    operation: export.operation.clone(),
                    export_id: outcome.export_id.clone(),
                    export_digest: outcome.review_digest.clone(),
                    read_environment: read.read.environment().into(),
                    write_environment: write.write.environment().into(),
                    resource: export.destination_resource.clone(),
                    epoch: write.epoch,
                    expires_at: now()?.checked_add(300).ok_or(PatchError::Invalid)?,
                    choice,
                    file_name: export.file_name.clone(),
                    capsule,
                    nonce: uuid::Uuid::new_v4().simple().to_string(),
                    observed: observed.clone(),
                },
            )
        };
        if uuid::Uuid::parse_str(&id)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != id
            || record.nonce.len() != 32
            || !record.nonce.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PatchError::Invalid);
        }
        if record.owner != export.owner
            || record.run != export.run
            || record.operation != export.operation
            || record.file_name != export.file_name
            || record.export_id != outcome.export_id
            || record.export_digest != outcome.review_digest
            || record.read_environment != read.read.environment()
            || record.write_environment != write.write.environment()
            || record.resource != export.destination_resource
            || record.epoch != write.epoch
            || record.choice != choice
            || record.observed != observed
            || record.expires_at <= now()?
            || record.capsule != format!(".orange-agent-tmp-retired-{id}")
        {
            return Err(JournalError::Conflict.into());
        }
        let body = serde_json::to_string(&record).map_err(|_| PatchError::Invalid)?;
        let digest = record.digest()?;
        self.validate_retirement(read, write)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM export_retirements WHERE retirement_id=?)",
                [&id],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if !exists {
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM export_retirements WHERE owner=?",
                    [&record.owner],
                    |r| r.get(0),
                )
                .map_err(JournalError::from)?;
            if count >= 1000 {
                return Err(PatchError::Limit);
            }
            tx.execute("INSERT INTO export_retirements(retirement_id,export_id,owner,request_key,host_epoch,body,digest,state) VALUES(?,?,?,?,?,?,?,'prepared')",params![id,record.export_id,record.owner,key,record.epoch,body,digest]).map_err(JournalError::from)?;
            tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_retirement_prepared')",params![record.run,record.operation]).map_err(JournalError::from)?;
        }
        snapshot.view()?;
        quiescence.check()?;
        if permit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        tx.commit().map_err(JournalError::from)?;
        Ok(PreparedExportRetirement {
            review: record.review(id)?,
            record,
            quiescence,
            write: permit,
            #[cfg(test)]
            before_capture: std::sync::Mutex::new(None),
        })
    }
    pub async fn approve_export_retirement(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRetirementGrant,
        prepared: PreparedExportRetirement,
        expected: &str,
    ) -> Result<ExportRetirementReceipt, PatchError> {
        if !matches!(
            tokio::runtime::Handle::current().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            return Err(PatchError::RecoveryRequired);
        }
        self.validate_retirement(read, write)?;
        prepared.live()?;
        let record = &prepared.record;
        if expected != prepared.review.digest
            || record.digest()? != expected
            || record.owner != read.read.owner()
            || record.export_id != write.export_id
            || record.read_environment != read.read.environment()
            || record.write_environment != write.write.environment()
            || record.epoch != write.epoch
            || record.expires_at <= now()?
        {
            return Err(JournalError::ApprovalRequired.into());
        }
        // Validate before creating any capsule, and again immediately before capture.
        let (export, _) = self.validate_retirement(read, write)?;
        let name = match record.choice {
            ExportRetirementChoice::Target => &export.file_name,
            ExportRetirementChoice::Staged => &export.temporary,
        };
        if observe_export_file(&prepared.write.inner.directory, name, &export.artifact)?
            != record.observed
        {
            return Err(PatchError::VersionChanged);
        }
        let id = prepared.review.retirement_id.clone();
        {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(JournalError::from)?;
            if record.expires_at <= now()? {
                return Err(JournalError::ApprovalRequired.into());
            }
            if tx.execute("UPDATE export_retirements SET state='dispatching' WHERE retirement_id=? AND owner=? AND state='prepared' AND body=? AND digest=? AND host_epoch=?",params![id,record.owner,serde_json::to_string(record).map_err(|_|PatchError::Invalid)?,expected,record.epoch]).map_err(JournalError::from)?!=1{return Err(JournalError::Conflict.into());}
            tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_retirement_dispatched')",params![record.run,record.operation]).map_err(JournalError::from)?;
            prepared.live()?;
            tx.commit().map_err(JournalError::from)?;
        }
        let result = tokio::task::block_in_place(|| {
            self.capture_export_retirement(read, write, prepared, &export)
        });
        if result.is_err() {
            let _=self.connection.execute("UPDATE export_retirements SET state='unknown' WHERE retirement_id=? AND state='dispatching'",[id]);
        }
        result
    }
    fn capture_export_retirement(
        &mut self,
        read: &ExportInspectionGrant,
        write: &ExportRetirementGrant,
        prepared: PreparedExportRetirement,
        export: &Record,
    ) -> Result<ExportRetirementReceipt, PatchError> {
        prepared.live()?;
        self.validate_retirement(read, write)?;
        let parent = &prepared.write.inner.directory;
        let record = &prepared.record;
        let id = &prepared.review.retirement_id;
        let name = match record.choice {
            ExportRetirementChoice::Target => &export.file_name,
            ExportRetirementChoice::Staged => &export.temporary,
        };
        if observe_export_file(parent, name, &export.artifact)? != record.observed {
            return Err(PatchError::VersionChanged);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        let original = parent.open_with(name, &options).map_err(io)?;
        let mut identity = authority::identity(parent, &original.metadata().map_err(io)?)?;
        identity.bind_security(
            orange_platform_files::security_metadata(&original)
                .map_err(io)?
                .digest(),
        );
        let ExportFileObservation::Regular {
            object_digest,
            bytes,
            sha256,
            ..
        } = &record.observed
        else {
            return Err(PatchError::Invalid);
        };
        if request_digest("export-inspected-object.v1", &identity)
            .map_err(|_| PatchError::Invalid)?
            != *object_digest
        {
            return Err(PatchError::VersionChanged);
        }
        rustix::fs::mkdirat(
            parent,
            &record.capsule,
            rustix::fs::Mode::from_raw_mode(0o700),
        )
        .map_err(|e| io(e.into()))?;
        let capsule = open_directory(parent, &[&record.capsule])?;
        let meta = capsule.dir_metadata().map_err(io)?;
        if meta.mode() & 0o077 != 0 {
            return Err(PatchError::RecoveryRequired);
        }
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No)
            .mode(0o600);
        let mut marker = capsule.open_with("_identity", &options).map_err(io)?;
        marker.write_all(record.nonce.as_bytes()).map_err(io)?;
        marker.sync_all().map_err(io)?;
        sync(&capsule)?;
        sync(parent)?;
        let binding = Capsule {
            device: meta.dev().to_string(),
            inode: meta.ino().to_string(),
            nonce_sha256: hex::encode(sha2::Sha256::digest(record.nonce.as_bytes())),
        };
        self.connection
            .execute(
                "INSERT INTO export_retirement_capsules(retirement_id,body,digest) VALUES(?,?,?)",
                params![
                    id,
                    serde_json::to_string(&binding).map_err(|_| PatchError::Invalid)?,
                    request_digest("export-retirement-capsule.v1", &binding)
                        .map_err(|_| PatchError::Invalid)?
                ],
            )
            .map_err(JournalError::from)?;
        prepared.live()?;
        self.validate_retirement(read, write)?;
        if observe_export_file(parent, name, &export.artifact)? != record.observed {
            return Err(PatchError::VersionChanged);
        }
        #[cfg(test)]
        if let Some(hook) = prepared.before_capture.lock().expect("test hook").take() {
            hook();
        }
        prepared.live()?;
        if record.expires_at <= now()? {
            return Err(JournalError::ApprovalRequired.into());
        }
        check_capsule(parent, &capsule, &record.capsule, &record.nonce, &binding)?;
        rustix::fs::renameat_with(
            parent,
            name,
            &capsule,
            "item",
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|e| io(e.into()))?;
        sync(&capsule)?;
        sync(parent)?;
        let named = capsule.symlink_metadata("item").map_err(io)?;
        let mut after = authority::identity(parent, &named)?;
        let moved = capsule
            .open_with("item", &{
                let mut o = OpenOptions::new();
                o.read(true).follow(FollowSymlinks::No).nonblock(true);
                o
            })
            .map_err(io)?;
        if named.nlink() != 1 || !same_file(&capsule, "item", &original)? {
            return Err(PatchError::RecoveryRequired);
        }
        after.bind_security(
            orange_platform_files::security_metadata(&moved)
                .map_err(io)?
                .digest(),
        );
        if !identity.matches_after_rename(&after) {
            return Err(PatchError::RecoveryRequired);
        }
        let seen = observe_export_file(&capsule, "item", &export.artifact)?;
        if !matches!(&seen,ExportFileObservation::Regular {bytes:b,sha256:s,matches_approved_artifact:true,..} if b==bytes&&s==sha256)
        {
            return Err(PatchError::RecoveryRequired);
        }
        if directory_identity(&open_directory(parent, &[&record.capsule])?)?
            != directory_identity(&capsule)?
        {
            return Err(PatchError::RecoveryRequired);
        }
        check_capsule(parent, &capsule, &record.capsule, &record.nonce, &binding)?;
        prepared.live()?;
        self.validate_retirement(read, write)?;
        let receipt = ExportRetirementReceipt {
            retirement_id: id.clone(),
            export_id: record.export_id.clone(),
            review_digest: prepared.review.digest.clone(),
            choice: record.choice,
            bytes: bytes.clone(),
            sha256: sha256.clone(),
            disposition: "retained_in_capsule".into(),
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        if tx.execute("UPDATE export_retirements SET state='retained',receipt=?,receipt_digest=? WHERE retirement_id=? AND state='dispatching'",params![serde_json::to_string(&receipt).map_err(|_|PatchError::Invalid)?,request_digest("export-retirement-receipt.v1",&receipt).map_err(|_|PatchError::Invalid)?,id]).map_err(JournalError::from)?!=1{return Err(PatchError::RecoveryRequired);}
        tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.export_retirement_retained')",params![record.run,record.operation]).map_err(JournalError::from)?;
        prepared.live()?;
        tx.commit().map_err(JournalError::from)?;
        Ok(receipt)
    }
}
fn sync(dir: &Dir) -> Result<(), PatchError> {
    dir.try_clone()
        .map_err(io)?
        .into_std_file()
        .sync_all()
        .map_err(io)
}

fn check_capsule(
    parent: &Dir,
    capsule: &Dir,
    name: &str,
    nonce: &str,
    binding: &Capsule,
) -> Result<(), PatchError> {
    let meta = capsule.dir_metadata().map_err(io)?;
    if meta.mode() & 0o077 != 0
        || meta.dev().to_string() != binding.device
        || meta.ino().to_string() != binding.inode
        || directory_identity(&open_directory(parent, &[name])?)? != directory_identity(capsule)?
    {
        return Err(PatchError::RecoveryRequired);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let marker = capsule.open_with("_identity", &options).map_err(io)?;
    let meta = marker.metadata().map_err(io)?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.len() != nonce.len() as u64
    {
        return Err(PatchError::RecoveryRequired);
    }
    let mut bytes = Vec::new();
    marker.take(65).read_to_end(&mut bytes).map_err(io)?;
    if bytes != nonce.as_bytes()
        || hex::encode(sha2::Sha256::digest(&bytes)) != binding.nonce_sha256
    {
        return Err(PatchError::RecoveryRequired);
    }
    Ok(())
}
impl Journal {
    pub fn export_retirement_outcome(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        id: &str,
    ) -> Result<Option<ExportRetirementOutcome>, PatchError> {
        if ![owner, run, operation, id].iter().all(|v| valid_id(v)) {
            return Err(PatchError::Invalid);
        }
        struct Row {
            export: String,
            body: String,
            digest: String,
            state: String,
            receipt: Option<String>,
            receipt_digest: Option<String>,
        }
        let row:Option<Row>=self.connection.query_row("SELECT r.export_id,r.body,r.digest,r.state,r.receipt,r.receipt_digest FROM export_retirements r JOIN patch_exports e USING(export_id) JOIN operations o ON o.operation_id=e.operation_id WHERE r.retirement_id=? AND r.owner=? AND e.owner=? AND o.owner=? AND o.run_id=? AND o.operation_id=?",params![id,owner,owner,owner,run,operation],|r|Ok(Row {export:r.get(0)?,body:r.get(1)?,digest:r.get(2)?,state:r.get(3)?,receipt:r.get(4)?,receipt_digest:r.get(5)?})).optional().map_err(JournalError::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.body.len() > 32 * 1024 {
            return Err(PatchError::Limit);
        }
        let record: Retirement =
            serde_json::from_str(&row.body).map_err(|_| PatchError::Invalid)?;
        let export = self
            .retention_export_outcome(owner, run, operation, &row.export)?
            .ok_or(PatchError::Invalid)?;
        if uuid::Uuid::parse_str(id)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != id
            || record.nonce.len() != 32
            || !record.nonce.bytes().all(|b| b.is_ascii_hexdigit())
            || !matches!(
                row.state.as_str(),
                "prepared" | "dispatching" | "retained" | "unknown"
            )
            || record.owner != owner
            || record.run != run
            || record.operation != operation
            || record.export_id != row.export
            || record.export_digest != export.review_digest
            || record.digest()? != row.digest
            || record.capsule != format!(".orange-agent-tmp-retired-{id}")
        {
            return Err(PatchError::Invalid);
        }
        if row.state == "retained" {
            let body = row.receipt.ok_or(PatchError::Invalid)?;
            if body.len() > 8192 {
                return Err(PatchError::Limit);
            }
            let receipt: ExportRetirementReceipt =
                serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            let ExportFileObservation::Regular { bytes, sha256, .. } = &record.observed else {
                return Err(PatchError::Invalid);
            };
            if receipt.retirement_id != id
                || receipt.export_id != row.export
                || receipt.review_digest != row.digest
                || receipt.choice != record.choice
                || receipt.bytes != *bytes
                || receipt.sha256 != *sha256
                || receipt.disposition != "retained_in_capsule"
                || Some(
                    request_digest("export-retirement-receipt.v1", &receipt)
                        .map_err(|_| PatchError::Invalid)?,
                ) != row.receipt_digest
            {
                return Err(PatchError::Invalid);
            }
        } else if row.receipt.is_some() || row.receipt_digest.is_some() {
            return Err(PatchError::Invalid);
        }
        Ok(Some(ExportRetirementOutcome {
            retirement_id: id.into(),
            export_id: row.export,
            review_digest: row.digest,
            state: row.state,
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportRetirementInspection {
    pub retirement_id: String,
    pub recorded_state: String,
    pub choice: ExportRetirementChoice,
    /// False means no durable capsule binding, not proof that no effect exists.
    pub capsule_bound: bool,
    pub retained: Option<ExportFileObservation>,
}
pub struct ExportRetirementInspectionReceipt {
    view: ExportRetirementInspection,
    read: Permit,
}
impl ExportRetirementInspectionReceipt {
    pub fn view(&self) -> Result<&ExportRetirementInspection, PatchError> {
        if self.read.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(&self.view)
    }
}
impl Journal {
    pub fn export_retirement_outcomes(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        export_id: &str,
        before: Option<&str>,
        limit: u16,
    ) -> Result<Vec<ExportRetirementOutcome>, PatchError> {
        if before.is_some_and(|id| !valid_id(id)) || !(1..=100).contains(&limit) {
            return Err(PatchError::Invalid);
        }
        self.retention_export_outcome(owner, run, operation, export_id)?
            .ok_or(WorkspaceError::Forbidden)?;
        let mut statement=self.connection.prepare("SELECT retirement_id FROM export_retirements WHERE owner=?1 AND export_id=?2 AND (?3 IS NULL OR retirement_id<?3) ORDER BY retirement_id DESC LIMIT ?4").map_err(JournalError::from)?;
        let ids = statement
            .query_map(params![owner, export_id, before, i64::from(limit)], |r| {
                r.get::<_, String>(0)
            })
            .map_err(JournalError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(JournalError::from)?;
        ids.iter()
            .map(|id| {
                self.export_retirement_outcome(owner, run, operation, id)?
                    .ok_or(PatchError::Invalid)
            })
            .collect()
    }
    pub async fn inspect_export_retirement(
        &self,
        read: &ExportInspectionGrant,
        id: &str,
    ) -> Result<ExportRetirementInspectionReceipt, PatchError> {
        let (export, parent) = self.validate_export_inspection(read)?;
        let outcome = self
            .export_retirement_outcome(&export.owner, &export.run, &export.operation, id)?
            .ok_or(WorkspaceError::Forbidden)?;
        if outcome.export_id != parent.export_id {
            return Err(WorkspaceError::Forbidden.into());
        }
        let body: String = self
            .connection
            .query_row(
                "SELECT body FROM export_retirements WHERE retirement_id=?",
                [id],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        let record: Retirement = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let binding: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT body,digest FROM export_retirement_capsules WHERE retirement_id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(JournalError::from)?;
        let binding = if let Some((body, digest)) = binding {
            if body.len() > 4096 {
                return Err(PatchError::Limit);
            }
            let capsule: Capsule = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if request_digest("export-retirement-capsule.v1", &capsule)
                .map_err(|_| PatchError::Invalid)?
                != digest
            {
                return Err(PatchError::Invalid);
            }
            Some(capsule)
        } else {
            None
        };
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
        let id = id.to_owned();
        let work = tokio::task::spawn_blocking(
            move || -> Result<ExportRetirementInspectionReceipt, PatchError> {
                let _slot = slot;
                if permit.inner.is_revoked() {
                    return Err(WorkspaceError::Revoked.into());
                }
                let retained = if let Some(binding) = &binding {
                    let capsule = open_directory(&permit.inner.directory, &[&record.capsule])?;
                    check_capsule(
                        &permit.inner.directory,
                        &capsule,
                        &record.capsule,
                        &record.nonce,
                        binding,
                    )?;
                    let value = observe_export_file(&capsule, "item", &export.artifact)?;
                    check_capsule(
                        &permit.inner.directory,
                        &capsule,
                        &record.capsule,
                        &record.nonce,
                        binding,
                    )?;
                    Some(value)
                } else {
                    None
                };
                let receipt = ExportRetirementInspectionReceipt {
                    view: ExportRetirementInspection {
                        retirement_id: id,
                        recorded_state: outcome.state,
                        choice: record.choice,
                        capsule_bound: binding.is_some(),
                        retained,
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
