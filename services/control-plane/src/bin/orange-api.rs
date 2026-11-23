use orange_control_plane::api_startup::{self, ApiSettings};
use orange_control_plane::telemetry::{self, ApiRole};
use orange_control_plane::{
    http::{ApiState, DevIdentity, router},
    store::Store,
};
use std::error::Error;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    api_startup::exit(run().await)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let settings = ApiSettings::load(ApiRole::Development)?;
    let identities: Vec<DevIdentity> = settings.identity()?;
    settings.validate_development(&identities)?;
    if settings.describe() {
        return Ok(());
    }
    let address = settings.bind;
    let store = Store::connect(&settings.database_url).await?;
    store.migrate().await?;
    let app = router(ApiState::development(store, identities)?);
    let (app, _metrics) =
        telemetry::configure_listener(app, ApiRole::Development, settings.metrics_bind).await?;
    let app = orange_control_plane::diagnostics::observe(
        app,
        ApiRole::Development,
        settings.diagnostic_log,
    );
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
