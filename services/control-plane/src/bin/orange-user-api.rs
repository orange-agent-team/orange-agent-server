#[path = "user_api/service.rs"]
mod service;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    orange_control_plane::api_startup::exit(
        service::run(
            #[cfg(unix)]
            orange_control_plane::repo_connector::registry::Sources::load_private,
        )
        .await,
    )
}
