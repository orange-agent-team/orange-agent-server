use orange_control_plane::{
    admin::{
        identity::{AdminIdentity, OidcConfig},
        routes,
    },
    store::Store,
};
use std::{env, error::Error, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("This candidate currently permits development deployment only".into());
    }
    let address: SocketAddr = env::var("ORANGE_ADMIN_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43102".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("Development admin API must remain loopback-only".into());
    }
    let config_path = env::var("ORANGE_ADMIN_CONFIG_FILE")?;
    let config: OidcConfig = serde_json::from_slice(&std::fs::read(config_path)?)?;
    if config.environment != "dev" {
        return Err("Development binary cannot authenticate production environment".into());
    }
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let mut identity = AdminIdentity::discover(store.pool().clone(), config).await?;
    if let Some(root) = env::var_os("ORANGE_ADMIN_SUPPORT_ROOT") {
        let reader = orange_control_plane::artifacts::ArtifactReader::open_local(
            store.pool().clone(),
            std::path::Path::new(&root),
        )
        .await?;
        identity = identity.with_support_reader(reader);
    }
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Development Admin API listening on {}; node intents/approvals enabled, device completion remains separate",
        listener.local_addr()?
    );
    axum::serve(listener, routes::router(identity))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
