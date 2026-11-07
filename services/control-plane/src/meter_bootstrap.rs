//! Explicit private server counter configuration; never supplied by Worker RPC.
use orange_model_runtime::meter_service::TrustedMeter;
use std::{
    error::Error,
    path::{Path, PathBuf},
};
#[cfg(unix)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    protocol: u32,
    runtime_directory: PathBuf,
    executable: PathBuf,
    executable_sha256: String,
    model: PathBuf,
    model_sha256: String,
    model_bytes: u64,
    model_version: String,
    context_tokens: u32,
}
#[cfg(unix)]
pub async fn load(path: &Path) -> Result<TrustedMeter, Box<dyn Error>> {
    let c: Config =
        serde_json::from_slice(&crate::worker::bootstrap::private_configuration(path)?)?;
    if c.protocol != 1 {
        return Err("unsupported trusted meter configuration".into());
    }
    Ok(TrustedMeter::launch(
        &c.runtime_directory,
        orange_model_runtime::supervisor::CpuProfile {
            socket_directory: None,
            executable: c.executable,
            executable_sha256: c.executable_sha256,
            model: c.model,
            model_sha256: c.model_sha256,
            model_bytes: c.model_bytes,
            model_version: c.model_version,
            context_tokens: c.context_tokens,
            startup_timeout: std::time::Duration::from_secs(120),
        },
    )
    .await?)
}
#[cfg(not(unix))]
pub async fn load(_path: &Path) -> Result<TrustedMeter, Box<dyn Error>> {
    Err("trusted native counter unavailable on this target".into())
}
