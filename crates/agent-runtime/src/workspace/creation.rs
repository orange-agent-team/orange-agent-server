//! Trusted-host create-only capability. Ordinary WorkspaceGrant cannot be
//! converted to this type, and it is never constructed from model arguments.
//! Model-loop and OS-consent integration remain separate from this boundary.
use super::*;
use crate::{Journal, JournalError, Operation};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::io::{Seek, Write};

const MAX_CREATE: usize = 64 * 1024;
static WRITE_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFileArgs {
    pub path: String,
    pub content: String,
}
impl CreateFileArgs {
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        components(&self.path, false)?;
        if self.content.len() > MAX_CREATE || self.content.contains('\0') {
            return Err(WorkspaceError::Limit);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String, WorkspaceError> {
        self.validate()?;
        request_digest("write_file.v1", self).map_err(|_| WorkspaceError::Invalid)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreatedFile {
    pub path: String,
    pub sha256: String,
    pub bytes: String,
}
#[derive(Clone)]
pub struct CreateFileGrant {
    grant: WorkspaceGrant,
    environment: String,
}
impl CreateFileGrant {
    pub fn environment(&self) -> &str {
        &self.environment
    }
    pub fn request_revoke(&self) {
        self.grant.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.grant.wait_drained().await;
    }
}
#[derive(Debug, thiserror::Error)]
pub enum CreateFileError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("destination already exists; it was not replaced")]
    AlreadyExists,
    #[error("publication outcome is not durably confirmed; never replay")]
    Unknown,
    #[error("atomic create publication is unavailable on this target")]
    Unsupported,
}
struct Publication {
    result: CreatedFile,
    _permit: Permit,
}
impl Journal {
    /// Explicit process-local write consent, supplied only by the trusted Host.
    /// It is not inferred from a readonly workspace, persisted across restart,
    /// or exposed to a model. Each effect additionally requires an approval.
    pub fn authorize_file_creation(
        &self,
        owner: &str,
        consent_id: &str,
        policy: &str,
        root: &Path,
    ) -> Result<CreateFileGrant, CreateFileError> {
        let mut private = vec![self.private_root.clone()];
        private.extend(self.extra_private_roots.clone());
        let grant = WorkspaceGrant::open(owner, consent_id, policy, root, &private)?;
        let environment = request_digest("create-only-workspace-v1", &grant.environment())
            .map_err(|_| WorkspaceError::Invalid)?;
        Ok(CreateFileGrant { grant, environment })
    }
    /// Complete response -> exact approval -> durable dispatch -> atomic new
    /// file -> durable receipt. Uncertain outcomes cannot be retried as fresh.
    pub async fn create_file(
        &mut self,
        grant: &CreateFileGrant,
        op: &Operation<'_>,
        args: CreateFileArgs,
    ) -> Result<CreatedFile, CreateFileError> {
        args.validate()?;
        if !op.requires_approval
            || op.tool_version != "write_file.v1"
            || op.argument_digest != args.digest()?
            || op.environment != grant.environment
            || op.owner != grant.grant.owner()
            || op.policy_version != grant.grant.policy()
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
        return Err(CreateFileError::Unsupported);
        let slot = WRITE_SLOTS
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkspaceError::Interrupted)?;
        let permit = grant
            .grant
            .permit(op.owner, grant.grant.environment(), op.policy_version)
            .await?;
        self.prepare(op)?;
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT result_json FROM operations WHERE operation_id=? AND state='succeeded'",
                [op.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(JournalError::from)?
            .flatten();
        if let Some(json) = existing {
            let receipt: CreatedFile =
                serde_json::from_str(&json).map_err(|_| JournalError::Invalid)?;
            if receipt.path != args.path
                || receipt.sha256 != hex::encode(Sha256::digest(args.content.as_bytes()))
                || receipt.bytes != args.content.len().to_string()
            {
                return Err(JournalError::Conflict.into());
            }
            // This returns no old file content and performs no replay, even if
            // the user subsequently edits or removes the created file.
            return Ok(receipt);
        }
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| WorkspaceError::Invalid)?
                .as_secs(),
        )
        .map_err(|_| WorkspaceError::Invalid)?;
        self.claim(op, now)?;
        let mut cancellation = CancellationGuard {
            inner: permit.inner.clone(),
            finished: false,
        };
        let work = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            publish(permit, args)
        });
        let result = tokio::time::timeout(grant.grant.inner.deadline, work).await;
        let result = match result {
            Ok(Ok(result)) => result,
            _ => Err(CreateFileError::Unknown),
        };
        match result {
            Ok(receipt) => {
                if grant.grant.is_revoked() {
                    return Err(CreateFileError::Unknown);
                }
                let digest = request_digest("created-file.v1", &receipt.result)
                    .map_err(|_| JournalError::Invalid)?;
                let body =
                    serde_json::to_string(&receipt.result).map_err(|_| JournalError::Invalid)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|_| CreateFileError::Unknown)?;
                if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=? WHERE operation_id=? AND owner=? AND state='dispatching' AND EXISTS(SELECT 1 FROM runs WHERE run_id=? AND active=1)",params![digest,body,op.id,op.owner,op.run]).map_err(|_| CreateFileError::Unknown)?!=1{return Err(CreateFileError::Unknown);}
                tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.succeeded')",params![op.run,op.id]).map_err(|_| CreateFileError::Unknown)?;
                tx.commit().map_err(|_| CreateFileError::Unknown)?;
                cancellation.finished = true;
                Ok(receipt.result)
            }
            Err(CreateFileError::Unknown) => Err(CreateFileError::Unknown),
            Err(error) => {
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(JournalError::from)?;
                tx.execute("UPDATE operations SET state='failed',error_code='CREATE_REJECTED' WHERE operation_id=? AND state='dispatching'",[op.id]).map_err(JournalError::from)?;
                tx.execute(
                    "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.failed')",
                    params![op.run, op.id],
                )
                .map_err(JournalError::from)?;
                tx.commit().map_err(JournalError::from)?;
                cancellation.finished = true;
                Err(error)
            }
        }
    }
}
#[cfg(unix)]
fn same_file(directory: &Dir, name: &str, opened: &std::fs::Metadata) -> bool {
    use cap_fs_ext::OsMetadataExt as CapMetadata;
    use std::os::unix::fs::MetadataExt as StdMetadata;
    directory.symlink_metadata(name).is_ok_and(|named| {
        named.is_file()
            && CapMetadata::dev(&named) == StdMetadata::dev(opened)
            && CapMetadata::ino(&named) == StdMetadata::ino(opened)
            && StdMetadata::nlink(opened) == 1
    })
}
fn verify_written(file: &mut std::fs::File, expected: &str) -> Result<(), CreateFileError> {
    file.rewind().map_err(|_| CreateFileError::Unknown)?;
    let mut actual = Vec::new();
    file.take((MAX_CREATE + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(|_| CreateFileError::Unknown)?;
    if actual != expected.as_bytes() {
        return Err(CreateFileError::Unknown);
    }
    Ok(())
}
fn publish(permit: Permit, args: CreateFileArgs) -> Result<Publication, CreateFileError> {
    if permit.inner.is_revoked() {
        return Err(WorkspaceError::Revoked.into());
    }
    let parts = components(&args.path, false)?;
    let (name, parents) = parts.split_last().ok_or(WorkspaceError::Invalid)?;
    let directory = open_directory(&permit.inner.directory, parents)?;
    let temporary = format!(".orange-agent-tmp-{}", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600).nonblock(true);
    }
    let mut file = directory
        .open_with(&temporary, &options)
        .map_err(WorkspaceError::Io)?
        .into_std();
    let result = (|| {
        file.write_all(args.content.as_bytes())
            .map_err(WorkspaceError::Io)?;
        file.sync_all().map_err(WorkspaceError::Io)?;
        verify_written(&mut file, &args.content)?;
        #[cfg(unix)]
        if !same_file(
            &directory,
            &temporary,
            &file.metadata().map_err(WorkspaceError::Io)?,
        ) {
            return Err(CreateFileError::Unknown);
        }

        if permit.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        rustix::fs::renameat_with(
            &directory,
            &temporary,
            &directory,
            *name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                CreateFileError::AlreadyExists
            } else {
                CreateFileError::Workspace(WorkspaceError::Io(error.into()))
            }
        })?;
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
        return Err(CreateFileError::Unsupported);
        #[cfg(unix)]
        if !same_file(
            &directory,
            name,
            &file.metadata().map_err(|_| CreateFileError::Unknown)?,
        ) {
            return Err(CreateFileError::Unknown);
        }
        verify_written(&mut file, &args.content)?;
        // From this point an error is uncertain, never a safe retry or a reason
        // to delete/overwrite the destination.
        directory
            .try_clone()
            .map_err(|_| CreateFileError::Unknown)?
            .into_std_file()
            .sync_all()
            .map_err(|_| CreateFileError::Unknown)?;
        if permit.inner.is_revoked() {
            return Err(CreateFileError::Unknown);
        }
        Ok(CreatedFile {
            path: args.path.clone(),
            sha256: hex::encode(Sha256::digest(args.content.as_bytes())),
            bytes: args.content.len().to_string(),
        })
    })();
    if result.is_err() && !matches!(result, Err(CreateFileError::Unknown)) {
        // Only our unpredictable staging name is removed, never destination.
        if directory.remove_file(&temporary).is_err() {
            return Err(CreateFileError::Unknown);
        }
    }
    result.map(|result| Publication {
        result,
        _permit: permit,
    })
}

#[cfg(test)]
mod tests;
