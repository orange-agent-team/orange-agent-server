//! Private qualification process. The released User API always uses the fixed provider registry.
#[path = "../src/bin/user_api/service.rs"]
mod service;
#[cfg(unix)]
fn loopback_sources(
    path: &std::path::Path,
) -> Result<orange_control_plane::repo_connector::registry::Sources, &'static str> {
    use orange_control_plane::repo_connector::{
        Provider, Registration, Repository,
        registry::{Registered, Sources},
    };
    use serde::Deserialize;
    use std::{net::SocketAddr, sync::Arc};
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        owner_id: String,
        provider: Provider,
        address: SocketAddr,
    }
    let input: Fixture =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| "Missing private fixture")?)
            .map_err(|_| "Invalid fixture")?;
    if !input.address.ip().is_loopback() {
        return Err("Loopback provider required");
    }
    let reg = Registration {
        provider: input.provider,
        repositories: vec![Repository {
            id: 17,
            path: "acme/notes".into(),
            file_prefixes: vec!["docs".into()],
            issues: false,
            changes: false,
        }],
    };
    let digest =
        orange_protocol::request_digest("repository.model.fixture.v1", &(&input.owner_id, &reg))
            .map_err(|_| "Fixture digest")?;
    let reader = reg
        .open_loopback_fixture(input.address, "local-read-token")
        .map_err(|_| "Fixture registration")?;
    let mut sources = Sources::default();
    sources.0.insert(
        input.provider.name().into(),
        Arc::new(Registered {
            owner: input.owner_id,
            digest,
            registration: reg,
            reader,
        }),
    );
    Ok(sources)
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    orange_control_plane::api_startup::exit(
        service::run(
            #[cfg(unix)]
            loopback_sources,
        )
        .await,
    )
}
