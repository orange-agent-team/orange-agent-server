#[cfg(unix)]
use super::private_read;
#[cfg(unix)]
use serde::Deserialize;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
#[derive(Deserialize)]
struct HostProfile {
    protocol: u32,
    runtime_directory: PathBuf,
    socket_directory: Option<PathBuf>,
    instance_id: String,
    model_version: String,
    model_sha256: String,
    executable_sha256: String,
    model_bytes: u64,
    context_tokens: u32,
}

/// A qualified template may use only the private process registered by the model host.
/// The key is read before this check so a concurrently replaced instance cannot inherit a new key.
#[cfg(unix)]
pub(super) fn check(
    key: &Path,
    socket: Option<&Path>,
    model_version: &str,
    context: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use orange_model_runtime::supervisor::ProcessRecord;
    use orange_protocol::local_model::{LocalModelIdentity, local_7b_pin};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let rejected = "qualified model process mismatch";
    let root = key.parent().ok_or(rejected)?;
    let root_meta = std::fs::symlink_metadata(root)?;
    if !root.is_absolute()
        || !root_meta.is_dir()
        || root_meta.permissions().mode() & 0o077 != 0
        || key.file_name().and_then(|n| n.to_str()) != Some("api.key")
    {
        return Err(rejected.into());
    }
    let profile: HostProfile = serde_json::from_slice(&private_read(&root.join("profile.json"))?)?;
    let live: ProcessRecord = serde_json::from_slice(&private_read(&root.join("process.json"))?)?;
    let socket = socket.ok_or(rejected)?;
    let ipc = profile
        .socket_directory
        .as_ref()
        .unwrap_or(&profile.runtime_directory);
    let socket_meta = std::fs::symlink_metadata(socket)?;
    let facts = LocalModelIdentity {
        alias: profile.model_version,
        model_sha256: profile.model_sha256,
        executable_sha256: profile.executable_sha256,
        context_tokens: profile.context_tokens,
    };
    if !socket_meta.file_type().is_socket()
        || socket.canonicalize()? != ipc.canonicalize()?.join("engine.sock")
        || profile.protocol != 1
        || profile.runtime_directory.canonicalize()? != root.canonicalize()?
        || !live.cpu_only
        || live.phase != "ready"
        || live.pid.is_none()
        || live.instance_id != profile.instance_id
        || facts.alias != model_version
        || live.model_version != facts.alias
        || live.model_sha256 != facts.model_sha256
        || live.executable_sha256 != facts.executable_sha256
        || facts.context_tokens != context
        || !facts.matches_local_7b().map_err(|_| rejected)?
        || profile.model_bytes != local_7b_pin().map_err(|_| rejected)?.bytes
    {
        return Err(rejected.into());
    }
    facts.connection_id().map_err(|_| rejected)?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn check(
    _: &Path,
    _: Option<&Path>,
    _: &str,
    _: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("qualified local CPU model service is unavailable on this target".into())
}
