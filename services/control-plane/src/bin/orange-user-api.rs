use orange_control_plane::api_startup::{self, ApiSettings};
use orange_control_plane::telemetry::{self, ApiRole};
use orange_control_plane::{
    auth::{
        routes,
        users::{UserIdentity, UserOidcConfig},
    },
    store::Store,
};
use std::{env, error::Error, net::SocketAddr};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    api_startup::exit(run().await)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let settings = ApiSettings::load(ApiRole::User)?;
    let config: UserOidcConfig = settings.identity()?;
    settings.validate_oidc(
        &config.environment,
        &config.client_id,
        &config.client_secret,
    )?;
    if settings.describe() {
        return Ok(());
    }
    let meter = if let Some(path) = env::var_os("ORANGE_METER_CONFIG") {
        Some(orange_control_plane::meter_bootstrap::load(std::path::Path::new(&path)).await?)
    } else {
        None
    };
    let result = serve(meter.clone(), settings, config).await;
    if let Some(meter) = meter {
        meter.shutdown().await?;
    }
    result
}
async fn serve(
    meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
    settings: ApiSettings,
    config: UserOidcConfig,
) -> Result<(), Box<dyn Error>> {
    if meter.is_some() && env::var_os("ORANGE_ARTIFACT_ROOT").is_none() {
        return Err("trusted pricing requires the private artifact writer".into());
    }
    let address = settings.bind;
    let store = Store::connect(&settings.database_url).await?;
    store.migrate().await?;
    let identity = UserIdentity::discover(store.pool().clone(), config).await?;
    #[cfg(unix)]
    let identity = if let Some(path) = env::var_os("ORANGE_PG_VIEW_SOURCES") {
        let mut identity = identity;
        identity.pg_sources =
            orange_control_plane::pg_view_sources::Sources::load_development_file(
                std::path::Path::new(&path),
            )?;
        identity
    } else {
        identity
    };
    #[cfg(unix)]
    let identity = if let Some(path) = env::var_os("ORANGE_S3_SOURCES") {
        let mut identity = identity;
        identity.s3_sources = orange_control_plane::s3_sources::Sources::load_development_file(
            std::path::Path::new(&path),
        )?;
        identity
    } else {
        identity
    };
    #[cfg(unix)]
    let identity = if let Some(path) = env::var_os("ORANGE_REPOSITORY_SOURCES") {
        let mut identity = identity;
        identity.repository_sources =
            orange_control_plane::repo_connector::registry::Sources::load_private(
                std::path::Path::new(&path),
            )?;
        identity
    } else {
        identity
    };
    #[cfg(unix)]
    let identity = if let Some(path) = env::var_os("ORANGE_NOTION_SOURCES") {
        let mut identity = identity;
        identity.notion_sources = orange_control_plane::notion::registry::Sources::load_private(
            std::path::Path::new(&path),
        )?;
        identity
    } else {
        identity
    };
    #[cfg(unix)]
    let identity = if let Some(path) = env::var_os("ORANGE_CONFLUENCE_SOURCES") {
        let mut identity = identity;
        identity.confluence_sources =
            orange_control_plane::confluence::registry::Sources::load_private(
                std::path::Path::new(&path),
            )?;
        identity
    } else {
        identity
    };
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
    #[cfg(unix)]
    let payments = env::var_os("ORANGE_STRIPE_TEST_CONFIG")
        .map(|path| {
            orange_control_plane::payments::Sandbox::load(std::path::Path::new(&path)).map(
                |provider| {
                    orange_control_plane::payments::Payments::new(identity.clone(), provider)
                },
            )
        })
        .transpose()?;
    let mut app = routes::router(identity.clone()).merge(routes::confluence_router(
        identity.clone(),
        artifacts.clone(),
    ));
    #[cfg(unix)]
    let audio = env::var_os("ORANGE_AUDIO_METER_CONFIG")
        .map(|path| {
            orange_control_plane::artifacts::AudioMeter::from_file(std::path::Path::new(&path))
        })
        .transpose()?;
    if env::var_os("ORANGE_AUDIO_METER_CONFIG").is_some() && artifacts.is_none() {
        return Err("Audio measurement requires the private artifact writer".into());
    }
    #[cfg(not(unix))]
    if env::var_os("ORANGE_AUDIO_METER_CONFIG").is_some() {
        return Err("No qualified audio decoder for this target".into());
    }
    #[cfg(unix)]
    if let Some(p) = &payments {
        app = app.merge(p.router());
    }
    if let Some(objects) = &artifacts {
        app = app.merge(orange_control_plane::artifacts::http::build_router(
            identity,
            objects.clone(),
            #[cfg(unix)]
            audio,
        ));
    }
    let (app, _metrics) =
        telemetry::configure_listener(app, ApiRole::User, settings.metrics_bind).await?;
    let app =
        orange_control_plane::diagnostics::observe(app, ApiRole::User, settings.diagnostic_log);
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
    let mut model_stop = stop.subscribe();
    let model_store = store.clone();
    let cloud_results = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {_=model_stop.changed()=>break,_=tick.tick()=>{
                let result=tokio::select!{_=model_stop.changed()=>break,result=routes::reconcile_cloud_models(model_store.pool())=>result};
                if result.is_err(){eprintln!("Cloud model reconciliation deferred");}
            }}
        }
    });
    let earning_store = store.clone();
    let mut earning_stop = stop.subscribe();
    let earning_reviews = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {_=earning_stop.changed()=>break,_=tick.tick()=>{
                let result=tokio::select!{_=earning_stop.changed()=>break,result=orange_control_plane::provider_earnings::prepare_reviews(earning_store.pool(),100)=>result};
                if result.is_err(){eprintln!("Mature development earnings remain pending review");}
            }}
        }
    });
    let mut expiry_stopped = stop.subscribe();
    let expiry_store = store.clone();
    let queue_expiry = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut cursor = orange_control_plane::store::QueueExpiryCursor::default();
        loop {
            tokio::select! {
                _=expiry_stopped.changed()=>break,
                _=tick.tick()=>{
                    let cloud=tokio::select!{
                        _=expiry_stopped.changed()=>break,
                        result=routes::expire_cloud_workspaces(expiry_store.pool())=>result,
                    };
                    if cloud.is_err(){eprintln!("Cloud workspace expiry deferred");}
                    let activation = tokio::select! {
                        _ = expiry_stopped.changed() => break,
                        result = orange_control_plane::admin::activate_due_text_prices(expiry_store.pool()) => result,
                    };
                    if activation.is_err() { eprintln!("Scheduled development price activation deferred"); }
                    let pruned=tokio::select!{
                        _=expiry_stopped.changed()=>break,
                        result=orange_control_plane::events::prune_old_notifications(expiry_store.pool(),8,128)=>result
                    };
                    if !matches!(pruned,Ok((_,0))) { eprintln!("Operational event retention requires review"); }
                    let result=tokio::select!{_=expiry_stopped.changed()=>break,result=expiry_store.expire_queued_ready(&mut cursor,100)=>result};
                    if !matches!(result,Ok((_,0))){eprintln!("Queue expiration remains unresolved; no execution cleanup is inferred");}
                }
            }
        }
    });
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
        "Development User API listening on {}; shared output Gateway and test merchant are opt-in",
        api_listener.local_addr()?
    );
    let signal_stop = stop.clone();
    #[cfg(unix)]
    let statements = payments.clone().map(|p|{
        let mut shutdown=stop.subscribe();
        tokio::spawn(async move{
            let mut tick=tokio::time::interval(std::time::Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop{tokio::select!{_=shutdown.changed()=>break,_=tick.tick()=>{
                if p.scan_statement_page().await.is_err(){eprintln!("Sandbox merchant statement pending reconciliation");}
            }}}
        })
    });
    #[cfg(unix)]
    let payment_work=payments.map(|p|{
        let mut shutdown=stop.subscribe();
        tokio::spawn(async move{let mut tick=tokio::time::interval(std::time::Duration::from_secs(3));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop{tokio::select!{_=shutdown.changed()=>break,_=tick.tick()=>{
                for _ in 0..8{if *shutdown.borrow(){return;}match p.process_one().await{Ok(true)=>{},Ok(false)=>break,Err(_)=>{eprintln!("Sandbox payment work remains pending reconciliation");break;}}}
                if !*shutdown.borrow() && p.process_refund().await.is_err(){eprintln!("Sandbox refund remains pending reconciliation");}
                if !*shutdown.borrow() && p.process_dispute().await.is_err(){eprintln!("Sandbox dispute remains pending reconciliation");}
            }}}
        })
    });
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
    earning_reviews.await?;
    queue_expiry.await?;
    cloud_results.await?;
    #[cfg(unix)]
    if let Some(task) = statements {
        task.await?;
    }
    #[cfg(unix)]
    if let Some(task) = payment_work {
        task.await?;
    }
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
