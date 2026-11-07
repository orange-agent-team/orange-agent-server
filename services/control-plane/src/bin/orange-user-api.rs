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
        return Err("development deployment required".into());
    }
    let meter = if let Some(path) = env::var_os("ORANGE_METER_CONFIG") {
        Some(orange_control_plane::meter_bootstrap::load(std::path::Path::new(&path)).await?)
    } else {
        None
    };
    let result = serve(meter.clone()).await;
    if let Some(meter) = meter {
        meter.shutdown().await?;
    }
    result
}
async fn serve(
    meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
) -> Result<(), Box<dyn Error>> {
    if meter.is_some() && env::var_os("ORANGE_ARTIFACT_ROOT").is_none() {
        return Err("trusted pricing requires the private artifact writer".into());
    }
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
            orange_control_plane::artifacts::ArtifactStore::open_local_with_meter(
                store.pool().clone(),
                std::path::Path::new(&root),
                bytes,
                records,
                meter,
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
    let api_listener = tokio::net::TcpListener::bind(address).await?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let mut http_stopped = stop.subscribe();
    let gateway_task = if let Some(path) = env::var_os("ORANGE_WORKER_GATEWAY_CONFIG") {
        let objects = artifacts
            .clone()
            .ok_or("Co-hosted output Gateway requires the existing artifact writer")?;
        let address: SocketAddr = env::var("ORANGE_WORKER_GATEWAY_BIND")
            .unwrap_or_else(|_| "127.0.0.1:43104".into())
            .parse()?;
        if !address.ip().is_loopback() {
            return Err("Development output Gateway must remain loopback".into());
        }
        let (id, tls) = orange_control_plane::worker::bootstrap::load(std::path::Path::new(&path))?;
        let sessions =
            orange_control_plane::worker::WorkerSessionStore::new(store.pool().clone(), id)?;
        let gateway_listener = tokio::net::TcpListener::bind(address).await?;
        eprintln!(
            "Development output Gateway listening on {}",
            gateway_listener.local_addr()?
        );
        let mut gateway_stopped = stop.subscribe();
        let gateway_stop = stop.clone();
        Some(tokio::spawn(async move {
            let result = orange_control_plane::worker::gateway::serve_with_output_store(
                gateway_listener,
                tls,
                sessions,
                objects,
                async move {
                    let _ = gateway_stopped.changed().await;
                },
            )
            .await;
            let _ = gateway_stop.send(true);
            result
        }))
    } else {
        None
    };
    let maintenance=artifacts.clone().map(|objects| tokio::spawn(async move {
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(60));
        loop {tokio::select! {
            _=stopped.changed()=>break,
            _=tick.tick()=>{
                let result=tokio::select! {_=stopped.changed()=>break,result=objects.collect(100)=>result};
                if result.is_err(){eprintln!("Artifact maintenance unavailable; unresolved objects remain accounted for");}
                let settlement=tokio::select! {_=stopped.changed()=>break,result=objects.settle_ready(100)=>result};
                if !matches!(settlement,Ok((_,0))){eprintln!("Unresolved development settlements remain pending");}
            }
        }}
    }));
    eprintln!(
        "Development User API listening on {}; shared output Gateway is optional; no payment enabled",
        api_listener.local_addr()?
    );
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = signal_stop.send(true);
    });
    let serving = axum::serve(api_listener, app)
        .with_graceful_shutdown(async move {
            let _ = http_stopped.changed().await;
        })
        .await;
    let _ = stop.send(true);
    let gateway_result = if let Some(task) = gateway_task {
        Some(task.await)
    } else {
        None
    };
    if let Some(task) = maintenance {
        task.await?;
    }
    if let Some(objects) = artifacts {
        objects.drain().await?;
    }
    signal.abort();
    serving?;
    if let Some(result) = gateway_result {
        result??;
    }
    Ok(())
}
