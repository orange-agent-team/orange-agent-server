use orange_control_plane::api_startup::{self, ApiSettings};
use orange_control_plane::telemetry::{self, ApiRole};
use orange_control_plane::{
    admin::{
        identity::{AdminIdentity, OidcConfig},
        routes,
    },
    store::Store,
};
use std::{env, error::Error};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    api_startup::exit(run().await)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let settings = ApiSettings::load(ApiRole::Admin)?;
    let config: OidcConfig = settings.identity()?;
    settings.validate_oidc(
        &config.environment,
        &config.client_id,
        &config.client_secret,
    )?;
    if settings.describe() {
        return Ok(());
    }
    let address = settings.bind;
    let store = Store::connect(&settings.database_url).await?;
    store.migrate().await?;
    let mut identity = AdminIdentity::discover(store.pool().clone(), config).await?;
    if let Some(path) = env::var_os("ORANGE_MODEL_REVIEW_PUBLIC_KEY_FILE") {
        let trust = orange_control_plane::admin::model_review::ModelReviewTrust::load(
            std::path::Path::new(&path),
        )?;
        identity = identity.with_model_review_trust(trust);
    }
    if let Some(root) = env::var_os("ORANGE_ADMIN_SUPPORT_ROOT") {
        let reader = orange_control_plane::artifacts::ArtifactReader::open_local(
            store.pool().clone(),
            std::path::Path::new(&root),
        )
        .await?;
        identity = identity.with_support_reader(reader);
    }
    let (app, _metrics) = telemetry::configure_listener(
        routes::router(identity),
        ApiRole::Admin,
        settings.metrics_bind,
    )
    .await?;
    let app =
        orange_control_plane::diagnostics::observe(app, ApiRole::Admin, settings.diagnostic_log);
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Development Admin API listening on {}; node intents/approvals enabled, device completion remains separate",
        listener.local_addr()?
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
