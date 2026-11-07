use orange_control_plane::{
    store::Store,
    worker::{
        WorkerSessionStore,
        gateway::{self, GatewayTls},
    },
};
use serde::Deserialize;
use std::{
    env,
    error::Error,
    io::Read,
    net::SocketAddr,
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
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("This candidate permits explicit development deployment only".into());
    }
    let address: SocketAddr = env::var("ORANGE_WORKER_GATEWAY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43104".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("Development Worker Gateway must remain loopback-only".into());
    }
    let config: Config = serde_json::from_slice(&read_bounded(
        Path::new(&env::var("ORANGE_WORKER_GATEWAY_CONFIG")?),
        true,
    )?)?;
    let tls = GatewayTls {
        server_certificate_pem: read_bounded(&config.server_certificate_file, false)?,
        server_private_key_pem: read_bounded(&config.server_key_file, true)?,
        client_ca_pem: read_bounded(&config.client_ca_file, false)?,
    };
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let sessions = WorkerSessionStore::new(store.pool().clone(), config.gateway_instance_id)?;
    let artifacts = if let Some(root) = env::var_os("ORANGE_ARTIFACT_ROOT") {
        Some(
            orange_control_plane::artifacts::ArtifactReader::open_local(
                store.pool().clone(),
                Path::new(&root),
            )
            .await?,
        )
    } else {
        None
    };
    let reader = artifacts.clone();
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Development Worker mTLS control on {}; model execution/enrollment not enabled",
        listener.local_addr()?
    );
    gateway::serve_with_artifacts(listener, tls, sessions, artifacts, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    if let Some(reader) = reader {
        reader.drain().await?;
    }
    Ok(())
}
