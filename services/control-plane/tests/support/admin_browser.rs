//! Serve one real, loopback-only Admin API to an isolated browser driver.
pub(crate) async fn run(
    app: axum::Router,
    node: &str,
    driver: &str,
    mut fixture: serde_json::Value,
) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Admin API");
    fixture["api"] = serde_json::json!(format!(
        "http://{}",
        listener.local_addr().expect("Admin port")
    ));
    let (stop, done) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .expect("Admin served");
    });
    let mut child = tokio::process::Command::new(node)
        .arg(driver)
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("browser process");
    let mut pipe = child.stdin.take().expect("fixture pipe");
    pipe.write_all(&serde_json::to_vec(&fixture).expect("fixture"))
        .await
        .expect("private fixture delivery");
    drop(pipe);
    let status = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait())
        .await
        .expect("browser deadline")
        .expect("browser exit");
    let _ = stop.send(());
    task.await.expect("Admin joined");
    assert!(status.success(), "real browser driver");
}
