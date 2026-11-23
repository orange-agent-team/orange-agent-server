use super::*;

async fn read(
    f: &InputFixture,
    headers: &axum::http::HeaderMap,
    query: &str,
) -> (u16, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut req = axum::http::Request::builder()
        .uri(format!(
            "/api/v1/owners/{}/jobs/{}/events{query}",
            f.owner, f.job
        ))
        .body(axum::body::Body::empty())
        .expect("request");
    *req.headers_mut() = headers.clone();
    let resp = app.oneshot(req).await.expect("router");
    let status = resp.status().as_u16();
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("bounded");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

#[tokio::test]
async fn owned_history_resets_after_retention_replays_only_digests_and_binds_real_attempts() {
    let f = setup_model_scope(
        "client-ay",
        &"b".repeat(64),
        &"c".repeat(64),
        "private-job-history-canary".into(),
        32,
        true,
    )
    .await;
    let outsider = setup("client-di").await;
    sqlx::query(
        "INSERT INTO memberships(organization_id,user_id,role,active) VALUES($1,$2,'admin',TRUE)",
    )
    .bind(&f.owner)
    .bind(&outsider.user)
    .execute(f.d.store.pool())
    .await
    .expect("authorized team viewer, different submitter");
    let (status, snapshot) = read(&f, &f.headers, "").await;
    assert_eq!(status, 200);
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["job"]["job_id"], f.job);
    let stream = snapshot["cursor"]["stream_id"].as_str().expect("stream");
    let resume = snapshot["cursor"]["after"].as_str().expect("cursor");
    let query = format!("?stream_id={stream}&after=0&limit=1");
    let (status, page) = read(&f, &f.headers, &query).await;
    assert_eq!(status, 200);
    assert_eq!(page["events"][0]["seq"], "1");
    assert_eq!(page["events"][0]["event_type"], "job.queued");
    assert!(page["events"][0]["attempt_id"].is_null());
    assert_eq!(read(&f, &f.headers, &query).await.1, page);
    assert_eq!(read(&f, &outsider.headers, &query).await.0, 404);
    assert_eq!(read(&f, &f.headers, "?after=1").await.0, 422);
    assert_eq!(
        read(&f, &f.headers, &format!("?stream_id={stream}&after=00"))
            .await
            .0,
        422
    );
    assert_eq!(
        read(
            &f,
            &f.headers,
            &format!("?stream_id={stream}&after=0&limit=65")
        )
        .await
        .0,
        422
    );
    assert_eq!(
        read(&f, &f.headers, "?after=0&stream_id=not-current")
            .await
            .1["kind"],
        "reset"
    );
    bind(&f).await;
    let reader = ArtifactReader::open_local(f.d.store.pool().clone(), &f.root)
        .await
        .expect("input reader");
    let server = Running::start_with_input(&f.d.store, Some(reader)).await;
    let (mut connection, mut node, mut journal, record) = receiver(&f, &server).await;
    let (mut broker, supervisor) = broker(&f).await;
    connection
        .reserve_execution(
            &mut journal,
            &mut broker,
            &supervisor,
            "cpu",
            &record.command_id,
        )
        .expect("local reservation");
    connection
        .authorize_execution(
            &mut journal,
            &mut node,
            "input-capabilities",
            &record.command_id,
        )
        .await
        .expect("durable start grant")
        .confirm(&mut journal, &mut broker, &supervisor)
        .expect("confirm start");
    let a = assignment(&f).await;
    let attempt = a.lease.as_ref().expect("lease").attempt_id.as_str();
    let (_, page) = read(
        &f,
        &f.headers,
        &format!("?stream_id={stream}&after={resume}"),
    )
    .await;
    let event = page["events"]
        .as_array()
        .expect("page")
        .iter()
        .find(|e| e["event_type"] == "job.start_granted")
        .expect("durable real attempt");
    assert_eq!(event["attempt_id"], attempt);
    assert_eq!(event["payload_digest"].as_str().expect("digest").len(), 64);
    assert!(
        event.get("payload").is_none() && !page.to_string().contains("private-job-history-canary")
    );
    let foreign_stream: String = sqlx::query_scalar(
        "SELECT stream_id FROM event_streams WHERE entity_kind='job' AND entity_id=$1",
    )
    .bind(&outsider.job)
    .fetch_one(f.d.store.pool())
    .await
    .expect("other stream");
    let bad=sqlx::query("INSERT INTO durable_events(stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id,attempt_id) VALUES($1,555,'job.start_granted',$2,'test',clock_timestamp(),-1,$3)")
        .bind(&foreign_stream).bind("a".repeat(64)).bind(attempt).execute(f.d.store.pool()).await.expect_err("different Job must not borrow attempt");
    assert_eq!(
        bad.as_database_error().and_then(|db| db.code()).as_deref(),
        Some("23514")
    );
    sqlx::query("UPDATE outbox o SET delivered_at=clock_timestamp() FROM durable_events e WHERE e.event_id=o.domain_event_id AND e.stream_id=$1 AND e.seq=1").bind(stream).execute(f.d.store.pool()).await.expect("ack fixture");
    orange_control_plane::events::EventStore::new(f.d.store.pool().clone())
        .prune_delivered(stream, 1)
        .await
        .expect("actual retention");
    let (_, reset) = read(&f, &f.headers, &query).await;
    assert_eq!(reset["kind"], "reset");
    assert_eq!(reset["job"]["execution_state"], "running");
    assert!(
        reset["cursor"]["after"]
            .as_str()
            .expect("covered state")
            .parse::<i64>()
            .expect("integer")
            >= 2
    );
    f.identity
        .revoke_self(&f.headers)
        .await
        .expect("actual logout");
    assert_eq!(read(&f, &f.headers, &query).await.0, 401);
    connection.close();
    server.stop().await;
}
