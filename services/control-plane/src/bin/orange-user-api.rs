use orange_control_plane::{
    auth::{
        routes,
        users::{UserIdentity, UserOidcConfig},
    },
    store::Store,
};
use std::{env, error::Error, net::SocketAddr};
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("This candidate currently permits development deployment only".into());
    }
    let address: SocketAddr = env::var("ORANGE_USER_BIND")
        .unwrap_or_else(|_| "127.0.0.1:43103".into())
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("Development user API must remain loopback-only".into());
    }
    let config: UserOidcConfig =
        serde_json::from_slice(&std::fs::read(env::var("ORANGE_USER_CONFIG_FILE")?)?)?;
    if config.environment != "dev" {
        return Err("Development binary cannot authenticate production environment".into());
    }
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    store.migrate().await?;
    let identity = UserIdentity::discover(store.pool().clone(), config).await?;
    let artifacts = if let Some(root) = env::var_os("ORANGE_ARTIFACT_ROOT") {
        let bytes = env::var("ORANGE_ARTIFACT_BYTE_LIMIT")
            .unwrap_or_else(|_| "1073741824".into())
            .parse()?;
        let records = env::var("ORANGE_ARTIFACT_RECORD_LIMIT")
            .unwrap_or_else(|_| "10000".into())
            .parse()?;
        Some(
            orange_control_plane::artifacts::ArtifactStore::open_local(
                store.pool().clone(),
                std::path::Path::new(&root),
                bytes,
                records,
            )
            .await?,
        )
    } else {
        None
    };
    let mut app = routes::router(identity.clone());
    if let Some(objects) = &artifacts {
        app = app.merge(orange_control_plane::artifacts::http::router(
            identity,
            objects.clone(),
        ));
    }
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let maintenance=artifacts.clone().map(|objects| tokio::spawn(async move {
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(60));
        loop {tokio::select! {
            _=stopped.changed()=>break,
            _=tick.tick()=>{
                let result=tokio::select! {_=stopped.changed()=>break,result=objects.collect(100)=>result};
                if result.is_err(){eprintln!("Artifact maintenance unavailable; unresolved objects remain accounted for");}
            }
        }}
    }));
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "User identity/organization API listening on {}; hosted Agent and payment routes are not enabled",
        listener.local_addr()?
    );
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = stop.send(true);
        })
        .await;
    if let Some(task) = maintenance {
        task.await?;
    }
    if let Some(objects) = artifacts {
        objects.drain().await?;
    }
    serving?;
    Ok(())
}
