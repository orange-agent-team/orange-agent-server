use orange_control_plane::{
    store::Store,
    worker::{WorkerSessionStore, gateway},
};
use std::{env, error::Error, net::SocketAddr, path::Path};
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("This candidate permits explicit development deployment only".into());
    }
    if let Some(command) = env::args_os().nth(1) {
        if command != "issue-one" {
            return Err("Unsupported Worker gateway operation".into());
        }
        #[cfg(unix)]
        if env::args_os().count() == 3 {
            let url = env::var("DATABASE_URL")?;
            let db = reqwest::Url::parse(&url).map_err(|_| "Invalid local signer database")?;
            if !matches!(db.scheme(), "postgres" | "postgresql")
                || !matches!(db.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                || db.query().is_some()
                || db.fragment().is_some()
            {
                return Err("Development signer requires a loopback database".into());
            }
            let path = env::args_os()
                .nth(2)
                .ok_or("private issuer configuration required")?;
            let store = Store::connect(&url).await?;
            store.migrate().await?;
            let receipt = orange_control_plane::worker::device_issuer::issue_one(
                store.pool(),
                Path::new(&path),
            )
            .await?;
            println!("{}", serde_json::json!({"prepared_request":receipt}));
            return Ok(());
        }
        return Err("Unsupported Worker gateway operation".into());
    }
    let address: SocketAddr = env::var("ORANGE_WORKER_GATEWAY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43104".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("Development Worker Gateway must remain loopback-only".into());
    }
    let (gateway_instance_id, tls) = orange_control_plane::worker::bootstrap::load(Path::new(
        &env::var("ORANGE_WORKER_GATEWAY_CONFIG")?,
    ))?;
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let sessions = WorkerSessionStore::new(store.pool().clone(), gateway_instance_id)?;
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
        "Development standalone Worker mTLS control on {}; output writes require the shared User API writer",
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
