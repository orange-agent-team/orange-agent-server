#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("ORANGE_ENV").as_deref() != Ok("development") {
        return Err("explicit local development environment required".into());
    }
    let config: orange_control_plane::cloud_node::Config =
        serde_json::from_slice(&std::fs::read(std::env::var("ORANGE_CLOUD_HOST_CONFIG")?)?)?;
    let node = orange_control_plane::cloud_node::Node::connect(config).await?;
    let (stop, signal) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop.send(true);
    });
    node.serve(signal).await?;
    Ok(())
}
#[cfg(not(unix))]
fn main() -> Result<(), &'static str> {
    Err("the local platform Host requires its Unix controller and dedicated Linux guest")
}
