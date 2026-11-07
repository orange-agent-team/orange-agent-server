//! Actual model and owned Worker, current-user HTTP stop and cloud cleanup.
use super::super::host_cases as host;
use super::*;
async fn cancel_http(c: &Case, key: &str) -> orange_protocol::JobView {
    let app =
        orange_control_plane::artifacts::http::router(c.f.identity.clone(), c.f.objects.clone());
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/owners/{}/jobs/{}/cancel",
            c.f.owner, c.f.job
        ))
        .body(axum::body::Body::from("{}"))
        .expect("HTTP");
    *request.headers_mut() = c.f.headers.clone();
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().expect("header"));
    request
        .headers_mut()
        .insert("idempotency-key", key.parse().expect("key"));
    let response = app.oneshot(request).await.expect("cancel route");
    assert_eq!(response.status(), 200);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("body"),
    )
    .expect("JobView")
}
#[tokio::test]
async fn actual_user_cancel_stops_model_then_releases_once_and_does_not_replay() {
    let mut c = setup_priced("client-de").await;
    c.f.request.messages = vec![orange_protocol::model::Message::plain(
        orange_protocol::model::Role::User,
        "Count from 1 to 10000, one number per line. Continue until every number has been written."
            .into(),
    )];
    c.f.request.max_output_tokens = 1024;
    replace_input(&mut c).await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let root = runtime();
    let path = host::configuration_port(&c.f, server.port, &profile(), root.path());
    host::enable_outputs(&path);
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("config")).expect("JSON");
    config["confirm_cleanup"] = serde_json::json!(true);
    std::fs::write(&path, serde_json::to_vec(&config).expect("config")).expect("write");
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    let quote = quote_http(&c, &quote_request(&c, 64)).await;
    submit(&mut c, &quote).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(bytes) = std::fs::read(root.path().join("model/process.json"))
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && v["phase"] == "ready"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual model is running");
    let requested = cancel_http(&c, "original-user-cancel").await;
    assert_eq!(requested.execution_state, "cancelling");
    assert_eq!(requested.settlement_state, "pending");
    assert!(requested.charge.is_none());
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    host::stopped(root.path());
    let terminal = cancel_http(&c, "original-user-cancel").await;
    assert_eq!(terminal.execution_state, "cancelled");
    assert_eq!(terminal.settlement_state, "settled");
    assert_eq!(terminal.charge.expect("zero charge").0.units(), 0);
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM job_usage_facts WHERE job_id=$1),(SELECT count(*) FROM job_cleanup_releases WHERE job_id=$1),(SELECT count(*) FROM worker_cpu_cleanup_receipts WHERE job_id=$1)").bind(&c.f.job).fetch_one(c.f.d.store.pool()).await.expect("actual facts");
    assert_eq!(counts, (0, 1, 1));
    let wallet: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&c.f.owner)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("wallet");
    assert_eq!(wallet, (1000, 0));
    host::terminate(&mut child).await;
    let original = std::fs::read(root.path().join("model/process.json")).expect("record");
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    host::terminate(&mut child).await;
    assert_eq!(
        std::fs::read(root.path().join("model/process.json")).expect("same record"),
        original
    );
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("meter shutdown");
}

#[tokio::test]
async fn late_user_cancel_preserves_delivered_result_and_original_settlement() {
    let mut c = setup_priced("client-dh").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let root = runtime();
    let path = host::configuration_port(&c.f, server.port, &profile(), root.path());
    host::enable_outputs(&path);
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("config")).expect("JSON");
    config["confirm_cleanup"] = serde_json::json!(true);
    std::fs::write(&path, serde_json::to_vec(&config).expect("config")).expect("write");
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    let quote = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &quote).await;
    host::event(&mut events, "response_committed").await;
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    let original =
        c.f.d
            .store
            .job(&c.f.owner, &c.f.job)
            .await
            .expect("original delivery");
    assert_eq!(original.execution_state, "succeeded");
    let charge = original.charge.expect("original charge").0.units();
    assert!(charge > 0);
    for _ in 0..2 {
        let late = cancel_http(&c, "late-cancel").await;
        assert_eq!(late.execution_state, "succeeded");
        assert_eq!(late.settlement_state, "settled");
        assert_eq!(late.charge.expect("retained charge").0.units(), charge);
    }
    let facts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM job_response_commits WHERE job_id=$1),(SELECT count(*) FROM job_settlements WHERE job_id=$1),(SELECT count(*) FROM job_cleanup_releases WHERE job_id=$1)").bind(&c.f.job).fetch_one(c.f.d.store.pool()).await.expect("facts");
    assert_eq!(facts, (1, 1, 0));
    let result =
        c.f.objects
            .job_result(&c.f.actor, &c.f.owner, &c.f.job)
            .await
            .expect("still authorized original output");
    assert_eq!(result.state, "available");
    host::terminate(&mut child).await;
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("meter");
}
