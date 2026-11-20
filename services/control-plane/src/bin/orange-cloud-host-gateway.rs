use orange_control_plane::{
    cloud_host::{self, HostTls},
    store::Store,
};
use std::{env, error::Error, net::SocketAddr, path::PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("development environment required".into());
    }
    let addr: SocketAddr = env::var("ORANGE_CLOUD_HOST_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43107".into())
        .parse()?;
    if !addr.ip().is_loopback() {
        return Err("development cloud Host gateway must be loopback".into());
    }
    let root = PathBuf::from(
        env::var_os("ORANGE_CLOUD_HOST_TLS_DIR").ok_or("platform Host TLS directory required")?,
    );
    let tls = HostTls {
        server_certificate_pem: std::fs::read(root.join("gateway.pem"))?,
        server_private_key_pem: std::fs::read(root.join("gateway.key"))?,
        platform_host_ca_pem: std::fs::read(root.join("platform-host-ca.pem"))?,
    };
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let reader = if let Some(root) = env::var_os("ORANGE_CLOUD_HOST_ARTIFACT_ROOT") {
        Some(
            orange_control_plane::artifacts::ArtifactReader::open_local(
                store.pool().clone(),
                std::path::Path::new(&root),
            )
            .await?,
        )
    } else {
        None
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    cloud_host::serve_with_storage(listener, store.pool().clone(), tls, reader.clone(), async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    if let Some(reader) = reader {
        reader.drain().await?;
    }
    Ok(())
}
