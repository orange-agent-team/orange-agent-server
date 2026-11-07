//! Shared private TLS bootstrap for standalone or co-hosted development Gateway.
use super::gateway::GatewayTls;
use serde::Deserialize;
use std::{
    error::Error,
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    gateway_instance_id: String,
    server_certificate_file: PathBuf,
    server_key_file: PathBuf,
    client_ca_file: PathBuf,
}
fn read_bounded(path: &Path, secret: bool) -> Result<Vec<u8>, Box<dyn Error>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return Err("regular bounded TLS/config file required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if secret && (metadata.mode() & 0o077 != 0 || metadata.nlink() != 1) {
            return Err("private 0600 secret/config file required".into());
        }
        if secret
            && std::fs::metadata(path.parent().ok_or("private parent required")?)?.mode() & 0o077
                != 0
        {
            return Err("secret/config parent directory must be private (0700)".into());
        }
    }
    let mut options = std::fs::File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err("regular file required".into());
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err("file limit".into());
    }
    Ok(bytes)
}
pub fn private_configuration(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    read_bounded(path, true)
}
pub fn load(path: &Path) -> Result<(String, GatewayTls), Box<dyn Error>> {
    let config: Config = serde_json::from_slice(&read_bounded(path, true)?)?;
    let tls = GatewayTls {
        server_certificate_pem: read_bounded(&config.server_certificate_file, false)?,
        server_private_key_pem: read_bounded(&config.server_key_file, true)?,
        client_ca_pem: read_bounded(&config.client_ca_file, false)?,
    };
    Ok((config.gateway_instance_id, tls))
}
