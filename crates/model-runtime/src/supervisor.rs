//! Owned CPU-only backend process. Callers must reserve this private directory
//! outside every workspace grant. No arbitrary command/argument or GPU interface.
use crate::llama::LlamaClient;
use orange_protocol::{valid_digest, valid_id};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::process::{Child, Command};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("invalid or nonprivate runtime profile")]
    Invalid,
    #[error("another model instance retains the execution lease")]
    Busy,
    #[error("unresolved prior process requires reconciliation")]
    RecoveryRequired,
    #[error("model/backend artifact integrity mismatch")]
    Integrity,
    #[error("model process did not become ready")]
    Startup,
    #[error("model startup cancelled after confirmed cleanup")]
    Cancelled,
    #[error("model process cleanup is not confirmed")]
    CleanupUnknown,
    #[error("runtime filesystem operation failed")]
    Io(#[from] std::io::Error),
}
/// Constructed by the trusted installer/profile registry, never model arguments.
pub struct CpuProfile {
    /// Trusted private IPC directory, optionally shorter than durable state.
    /// Contains only the socket and a non-secret binding to this state root.
    pub socket_directory: Option<PathBuf>,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub model: PathBuf,
    pub model_sha256: String,
    pub model_bytes: u64,
    pub model_version: String,
    pub context_tokens: u32,
    pub startup_timeout: Duration,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRecord {
    pub instance_id: String,
    pub model_version: String,
    pub executable_sha256: String,
    pub model_sha256: String,
    pub phase: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub cpu_only: bool,
}
pub struct ManagedCpuModel {
    child: Child,
    root: PathBuf,
    socket: PrivateEndpoint,
    record: ProcessRecord,
    lease: Option<Arc<File>>,
    client: Option<LlamaClient>,
    stopped: bool,
}
impl ManagedCpuModel {
    pub async fn launch(root: &Path, profile: CpuProfile) -> Result<Self, ProcessError> {
        let (_keep, receiver) = tokio::sync::watch::channel(false);
        Self::launch_with_cancel(root, profile, receiver).await
    }
    pub async fn launch_with_cancel(
        root: &Path,
        profile: CpuProfile,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Self, ProcessError> {
        Self::launch_from_host(root, profile, cancellation, None).await
    }
    /// Trusted supervisor may preallocate an instance identity to bind an OS job
    /// and its private control channel to this exact model startup. Never reuse it.
    pub async fn launch_from_host(
        root: &Path,
        profile: CpuProfile,
        mut cancellation: tokio::sync::watch::Receiver<bool>,
        instance_id: Option<String>,
    ) -> Result<Self, ProcessError> {
        if *cancellation.borrow() || cancellation.has_changed().is_err() {
            return Err(ProcessError::Cancelled);
        }
        if !root.is_absolute()
            || instance_id.as_ref().is_some_and(|id| !valid_id(id))
            || !profile.executable.is_absolute()
            || !profile.model.is_absolute()
            || root.is_symlink()
            || !valid_id(&profile.model_version)
            || !valid_digest(&profile.executable_sha256)
            || !valid_digest(&profile.model_sha256)
            || profile.model_bytes < 4
            || profile.model_bytes > 64 * 1024 * 1024 * 1024
            || !(256..=32768).contains(&profile.context_tokens)
            || profile.startup_timeout < Duration::from_secs(1)
            || profile.startup_timeout > Duration::from_secs(120)
        {
            return Err(ProcessError::Invalid);
        }
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
            return Err(ProcessError::Invalid);
        }
        let root = root.canonicalize()?;
        let socket = PrivateEndpoint::open(&root, profile.socket_directory.as_deref())?;
        let lease = Arc::new(private_file(&root.join("engine.lease"), false)?);
        lease.try_lock().map_err(|_| ProcessError::Busy)?;
        let prior = root.join("process.json");
        if fs::symlink_metadata(&prior).is_ok() {
            let bytes = private_read(&prior, 16 * 1024)?;
            let record: ProcessRecord =
                serde_json::from_slice(&bytes).map_err(|_| ProcessError::RecoveryRequired)?;
            if record.phase != "stopped"
                || !record.cpu_only
                || !valid_id(&record.instance_id)
                || !valid_id(&record.model_version)
                || !valid_digest(&record.executable_sha256)
                || !valid_digest(&record.model_sha256)
            {
                return Err(ProcessError::RecoveryRequired);
            }
            if instance_id.as_deref() == Some(record.instance_id.as_str()) {
                return Err(ProcessError::Invalid);
            }
        }
        let hash_lease = lease.clone();
        let profile = tokio::task::spawn_blocking(move || {
            let _lease = hash_lease;
            verify_file(
                &profile.executable,
                &profile.executable_sha256,
                None,
                128 * 1024 * 1024,
                false,
            )?;
            verify_file(
                &profile.model,
                &profile.model_sha256,
                Some(profile.model_bytes),
                64 * 1024 * 1024 * 1024,
                true,
            )?;
            Ok::<_, ProcessError>(profile)
        })
        .await
        .map_err(|_| ProcessError::Integrity)??;
        if *cancellation.borrow() || cancellation.has_changed().is_err() {
            return Err(ProcessError::Cancelled);
        }
        if fs::symlink_metadata(&socket.path).is_ok() {
            // A confirmed shutdown removes the socket before its stopped receipt.
            // Any remaining endpoint is unresolved; never unlink a live listener.
            return Err(ProcessError::RecoveryRequired);
        }
        let key =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        let key_path = root.join("api.key");
        write_private(&key_path, key.as_bytes())?;
        let record = ProcessRecord {
            instance_id: instance_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            model_version: profile.model_version.clone(),
            executable_sha256: profile.executable_sha256,
            model_sha256: profile.model_sha256,
            phase: "starting".into(),
            pid: None,
            exit_code: None,
            cpu_only: true,
        };
        save_record(&root, &record)?;
        let child = Command::new(&profile.executable)
            .args(["--model"])
            .arg(&profile.model)
            .args(["--host"])
            .arg(&socket.path)
            .args(["--api-key-file"])
            .arg(&key_path)
            .args([
                "--alias",
                &profile.model_version,
                "--ctx-size",
                &profile.context_tokens.to_string(),
                "--threads",
                "2",
                "--parallel",
                "1",
                "--n-gpu-layers",
                "0",
            ])
            .args([
                "--threads-http",
                "2",
                "--no-ui",
                "--no-ui-mcp-proxy",
                "--jinja",
                "--no-context-shift",
            ])
            .current_dir(&root)
            .env_clear()
            // The backend retains stdin; this inherited lease prevents another
            // instance after parent death. It is not a parent-crash cleanup claim.
            .stdin(Stdio::from(lease.try_clone()?))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                fs::remove_file(&key_path)?;
                let mut failed = record;
                failed.phase = "stopped".into();
                save_record(&root, &failed)?;
                return Err(error.into());
            }
        };
        let mut managed = Self {
            child,
            root,
            socket,
            record,
            lease: Some(lease),
            client: None,
            stopped: false,
        };
        managed.record.pid = managed.child.id();
        save_record(&managed.root, &managed.record)?;
        let ready = tokio::select! {
            _=cancellation.changed()=>None,
            result=tokio::time::timeout(profile.startup_timeout, async {
            loop {
                if managed.child.try_wait()?.is_some() {
                    return Err(ProcessError::Startup);
                }
                managed.socket.observe()?;
                if let Ok(client) = LlamaClient::private_socket(
                    &managed.socket.path,
                    key.clone(),
                    profile.model_version.clone(),
                    profile.context_tokens,
                ) && client.health().await.is_ok()
                {
                    return Ok(client);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })=>Some(result),
        };
        if ready.is_none() || *cancellation.borrow() || cancellation.has_changed().is_err() {
            managed.shutdown().await?;
            return Err(ProcessError::Cancelled);
        }
        match ready {
            Some(Ok(Ok(client))) => {
                managed.client = Some(client);
                managed.record.phase = "ready".into();
                save_record(&managed.root, &managed.record)?;
                Ok(managed)
            }
            _ => {
                managed.shutdown().await?;
                Err(ProcessError::Startup)
            }
        }
    }
    pub fn record(&self) -> &ProcessRecord {
        &self.record
    }
    /// Refresh the owned-child exit status before presenting a live status.
    pub fn poll_status(&mut self) -> Result<ProcessRecord, ProcessError> {
        if !self.stopped
            && self.record.phase == "ready"
            && let Some(status) = self.child.try_wait()?
        {
            self.client.take();
            self.record.phase = "draining".into();
            save_record(&self.root, &self.record)?;
            self.finish(status)?;
        }
        Ok(self.record.clone())
    }
    /// Borrowed access cannot outlive the owned process or race its shutdown.
    pub fn client(&mut self) -> Result<&LlamaClient, ProcessError> {
        self.poll_status()?;
        if self.stopped || self.record.phase != "ready" {
            return Err(ProcessError::RecoveryRequired);
        }
        self.client.as_ref().ok_or(ProcessError::Startup)
    }
    pub async fn shutdown(&mut self) -> Result<(), ProcessError> {
        if self.stopped {
            return Ok(());
        }
        self.record.phase = "draining".into();
        save_record(&self.root, &self.record)?;
        self.client.take();
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None => {
                self.child.start_kill()?;
                tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                    .await
                    .map_err(|_| ProcessError::CleanupUnknown)??
            }
        };
        self.finish(status)
    }
    fn finish(&mut self, status: ExitStatus) -> Result<(), ProcessError> {
        self.socket.remove_owned()?;
        fs::remove_file(self.root.join("api.key"))?;
        self.record.phase = "stopped".into();
        self.record.exit_code = status.code();
        save_record(&self.root, &self.record)?;
        self.stopped = true;
        self.lease.take();
        Ok(())
    }
}
struct PrivateEndpoint {
    path: PathBuf,
    parent_identity: (u64, u64),
    identity: Option<(u64, u64)>,
}
impl PrivateEndpoint {
    fn open(root: &Path, selected: Option<&Path>) -> Result<Self, ProcessError> {
        let directory = selected.unwrap_or(root);
        let metadata = fs::symlink_metadata(directory)?;
        if !directory.is_absolute() || !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
            return Err(ProcessError::Invalid);
        }
        let directory = directory.canonicalize()?;
        let path = directory.join("engine.sock");
        if path.as_os_str().as_bytes().len() > 100 {
            return Err(ProcessError::Invalid);
        }
        if directory != root {
            // Different durable roots must never race to bind or unlink the
            // same short endpoint. This immutable binding also survives helper
            // death before the backend has created its socket. The backend keeps
            // the durable root's execution lease through inherited stdin.
            let state = fs::metadata(root)?;
            let mut digest = Sha256::new();
            digest.update(b"orange-private-endpoint-v1\0");
            digest.update(root.as_os_str().as_bytes());
            digest.update(state.dev().to_be_bytes());
            digest.update(state.ino().to_be_bytes());
            let binding = hex::encode(digest.finalize());
            let file = directory.join("state-binding");
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&file)
            {
                Ok(mut opened) => {
                    opened.write_all(binding.as_bytes())?;
                    opened.sync_all()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if private_read(&file, 64)? != binding.as_bytes() {
                        return Err(ProcessError::RecoveryRequired);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self {
            path,
            parent_identity: (metadata.dev(), metadata.ino()),
            identity: None,
        })
    }
    fn check_parent(&self) -> Result<(), ProcessError> {
        let parent = self.path.parent().ok_or(ProcessError::Invalid)?;
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.mode() & 0o077 != 0
            || (metadata.dev(), metadata.ino()) != self.parent_identity
        {
            return Err(ProcessError::CleanupUnknown);
        }
        Ok(())
    }
    fn observe(&mut self) -> Result<(), ProcessError> {
        self.check_parent()?;
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() {
                    return Err(ProcessError::CleanupUnknown);
                }
                let identity = (metadata.dev(), metadata.ino());
                if self.identity.is_some_and(|previous| previous != identity) {
                    return Err(ProcessError::CleanupUnknown);
                }
                self.identity = Some(identity);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn remove_owned(&mut self) -> Result<(), ProcessError> {
        self.observe()?;
        if self.identity.is_some() {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
impl Drop for ManagedCpuModel {
    fn drop(&mut self) {
        if !self.stopped {
            self.record.phase = "unknown".into();
            let _ = save_record(&self.root, &self.record);
            // Never synthesize a stopped receipt from Drop or an RPC cancellation.
            let _ = self.child.start_kill();
        }
    }
}
fn private_file(path: &Path, truncate: bool) -> Result<File, ProcessError> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file()
            || meta.file_type().is_symlink()
            || meta.nlink() != 1
            || meta.mode() & 0o077 != 0)
    {
        return Err(ProcessError::Invalid);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
        return Err(ProcessError::Invalid);
    }
    if truncate {
        file.set_len(0)?;
    }
    Ok(file)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ProcessError> {
    let mut file = private_file(path, true)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn private_read(path: &Path, limit: u64) -> Result<Vec<u8>, ProcessError> {
    let file = private_file(path, false)?;
    if file.metadata()?.len() > limit {
        return Err(ProcessError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(ProcessError::Invalid);
    }
    Ok(bytes)
}
fn save_record(root: &Path, record: &ProcessRecord) -> Result<(), ProcessError> {
    let bytes = serde_json::to_vec(record).map_err(|_| ProcessError::Invalid)?;
    let temporary = root.join("process.new");
    write_private(&temporary, &bytes)?;
    fs::rename(temporary, root.join("process.json"))?;
    File::open(root)?.sync_all()?;
    Ok(())
}
fn verify_file(
    path: &Path,
    expected: &str,
    bytes: Option<u64>,
    maximum: u64,
    gguf: bool,
) -> Result<(), ProcessError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.len() > maximum
        || bytes.is_some_and(|n| n != metadata.len())
    {
        return Err(ProcessError::Integrity);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(ProcessError::Integrity);
    }
    let mut sha = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut read = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if read == 0 && gguf && (count < 4 || &buffer[..4] != b"GGUF") {
            return Err(ProcessError::Integrity);
        }
        read += count as u64;
        if read > maximum {
            return Err(ProcessError::Integrity);
        }
        sha.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    if read != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
        || hex::encode(sha.finalize()) != expected
    {
        return Err(ProcessError::Integrity);
    }
    Ok(())
}
