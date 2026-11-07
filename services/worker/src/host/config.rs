use super::*;
use serde::Deserialize;
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub protocol: u32,
    pub environment: String,
    pub state_directory: PathBuf,
    pub endpoint: String,
    pub server_name: String,
    pub server_ca: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub node_id: String,
    pub allow_remote_contribution: bool,
    #[serde(default = "default_output")]
    pub commit_outputs: bool,
    #[serde(default = "default_output")]
    pub confirm_cleanup: bool,
    pub profile_id: String,
    pub profile: ProfileConfig,
}
fn default_output() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub model: PathBuf,
    pub model_sha256: String,
    pub model_bytes: u64,
    pub model_version: String,
    pub context_tokens: u32,
    pub startup_timeout_seconds: u64,
}
impl ProfileConfig {
    pub(super) fn model(&self) -> orange_model_runtime::supervisor::CpuProfile {
        orange_model_runtime::supervisor::CpuProfile {
            socket_directory: None,
            executable: self.executable.clone(),
            executable_sha256: self.executable_sha256.clone(),
            model: self.model.clone(),
            model_sha256: self.model_sha256.clone(),
            model_bytes: self.model_bytes,
            model_version: self.model_version.clone(),
            context_tokens: self.context_tokens,
            startup_timeout: Duration::from_secs(self.startup_timeout_seconds),
        }
    }
}
pub(super) fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}
pub(super) fn private_directory(path: &Path) -> Result<PathBuf, HostError> {
    let m = std::fs::symlink_metadata(path)?;
    if !path.is_absolute()
        || !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != uid()
        || m.mode() & 0o077 != 0
    {
        return Err(HostError::PrivateState);
    }
    Ok(path.canonicalize()?)
}
pub(super) fn private_file(path: &Path, max: u64) -> Result<Vec<u8>, HostError> {
    private_directory(path.parent().ok_or(HostError::PrivateState)?)?;
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.nlink() != 1 || m.mode() & 0o077 != 0 || m.len() > max
    {
        return Err(HostError::PrivateState);
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(HostError::PrivateState);
    }
    Ok(bytes)
}
impl WorkerConfig {
    pub fn read(path: &Path) -> Result<Self, HostError> {
        let config: Self = serde_json::from_slice(&private_file(path, 32 * 1024)?)
            .map_err(|_| HostError::Configuration)?;
        config.validate()?;
        Ok(config)
    }
    pub(super) fn validate(&self) -> Result<(), HostError> {
        let endpoint = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|_| HostError::Configuration)?;
        let uri = endpoint.uri();
        if self.protocol != 1
            || self.environment != "development"
            || !self.allow_remote_contribution
            || uri.scheme_str() != Some("https")
            || !matches!(
                uri.host(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            )
            || uri.path() != "/"
            || uri.query().is_some()
            || uri.authority().is_none_or(|v| v.as_str().contains('@'))
            || !orange_protocol::valid_id(&self.node_id)
            || !orange_protocol::valid_id(&self.profile_id)
            || !orange_protocol::valid_id(&self.profile.model_version)
            || !orange_protocol::valid_digest(&self.profile.model_sha256)
            || !orange_protocol::valid_digest(&self.profile.executable_sha256)
            || !(512..=32768).contains(&self.profile.context_tokens)
            || !(1..=120).contains(&self.profile.startup_timeout_seconds)
            || self.profile.model_bytes == 0
            || !self.profile.model.is_absolute()
            || !self.profile.executable.is_absolute()
        {
            return Err(HostError::Configuration);
        }
        private_directory(&self.state_directory)?;
        Ok(())
    }
    pub(super) fn control(&self, digest: &str) -> Result<ControlConfig, HostError> {
        self.validate()?;
        Ok(ControlConfig {
            endpoint: self.endpoint.clone(),
            server_name: self.server_name.clone(),
            server_ca_pem: private_file(&self.server_ca, 64 * 1024)?,
            certificate_pem: private_file(&self.certificate, 64 * 1024)?,
            private_key_pem: private_file(&self.private_key, 64 * 1024)?,
            node_id: self.node_id.clone(),
            capabilities_digest: digest.into(),
        })
    }
}
