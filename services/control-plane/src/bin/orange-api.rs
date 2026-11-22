use orange_control_plane::telemetry::{self, ApiRole};
use orange_control_plane::{
    http::{ApiState, DevIdentity, router},
    store::Store,
};
use std::{env, error::Error, net::SocketAddr, path::Path};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("This slice only supports explicit development mode; production authentication is not implemented".into());
    }
    let address: SocketAddr = env::var("ORANGE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43100".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("Development API must bind a loopback address".into());
    }
    let identities_path = env::var("ORANGE_DEV_IDENTITIES_FILE")?;
    if !Path::new(&identities_path).is_file() {
        return Err("Development identity file missing".into());
    }
    let identities: Vec<DevIdentity> = serde_json::from_slice(&std::fs::read(identities_path)?)?;
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let app = router(ApiState::development(store, identities)?);
    let (app, _metrics) = telemetry::configure(app, ApiRole::Development).await?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Orange development API listening on {}; no production or payment routes enabled",
        listener.local_addr()?
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
