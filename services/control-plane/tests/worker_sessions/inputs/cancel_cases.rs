//! Actual HTTP/PG cancellation; cleanup evidence here is a declared CPU fixture.
use super::*;
async fn cancel(
    f: &InputFixture,
    key: &str,
    headers: axum::http::HeaderMap,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/owners/{}/jobs/{}/cancel", f.owner, f.job))
        .body(axum::body::Body::from(body.to_string()))
        .expect("request");
    *request.headers_mut() = headers;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().expect("header"));
    request
        .headers_mut()
        .insert("idempotency-key", key.parse().expect("key"));
    let response = app.oneshot(request).await.expect("actual route");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .expect("body");
    (status, serde_json::from_slice(&body).expect("JSON"))
}
async fn balance(f: &InputFixture) -> (i64, i64) {
    sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
        .bind(&f.owner)
        .fetch_one(f.d.store.pool())
        .await
        .expect("wallet")
}
#[tokio::test]
async fn queued_cancel_checks_current_actor_csrf_and_strict_body_then_releases_once() {
    let f = setup("client-da").await;
    let other = setup("client-db").await;
    let empty = serde_json::json!({});
    assert_eq!(
        cancel(&f, "cancel-queued", other.headers.clone(), empty.clone())
            .await
            .0,
        403
    );
    let mut bad = f.headers.clone();
    bad.remove("x-csrf-token");
    assert_eq!(cancel(&f, "cancel-queued", bad, empty.clone()).await.0, 403);
    assert_eq!(
        cancel(
            &f,
            "cancel-queued",
            f.headers.clone(),
            serde_json::json!({"refund":"1000"})
        )
        .await
        .0,
        422
    );
    assert_eq!(balance(&f).await, (900, 100));
    let (a, b) = tokio::join!(
        cancel(&f, "cancel-queued", f.headers.clone(), empty.clone()),
        cancel(&f, "cancel-queued", f.headers.clone(), empty.clone())
    );
    assert_eq!((a.0, b.0), (200, 200));
    assert_eq!(a.1["execution_state"], "cancelled");
    assert_eq!(a.1["settlement_state"], "settled");
    assert_eq!(a.1["charge"], "0");
    assert_eq!(balance(&f).await, (1000, 0));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(format!("release:{}", f.job))
            .fetch_one(f.d.store.pool())
            .await
            .expect("ledger");
    assert_eq!(count, 1);
    sqlx::query("UPDATE user_sessions SET revoked=true WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("revoke");
    assert_eq!(
        cancel(&f, "cancel-queued", f.headers.clone(), empty)
            .await
            .0,
        401,
        "replay cannot bypass current auth"
    );
    f.objects.drain().await.expect("drain");
    other.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn running_cancel_keeps_hold_and_attempt_until_independent_cleanup() {
    let (f, server, c, r) = super::cleanup_cases::setup_cleanup("client-dc", true).await;
    let empty = serde_json::json!({});
    let first = cancel(&f, "cancel-running", f.headers.clone(), empty.clone()).await;
    assert_eq!(first.0, 200);
    assert_eq!(first.1["execution_state"], "cancelling");
    assert_eq!(first.1["settlement_state"], "pending");
    assert!(first.1["charge"].is_null());
    assert_eq!(balance(&f).await, (900, 100));
    assert_eq!(
        cancel(&f, "cancel-running", f.headers.clone(), empty.clone())
            .await
            .0,
        200
    );
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("source deleted");
    assert_eq!(
        cancel(&f, "cancel-running", f.headers.clone(), empty.clone())
            .await
            .0,
        200,
        "stop does not read deleted source"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("attempt");
    assert_eq!(state, "started");
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.entity_id=$1 AND e.event_type='job.cancel_requested'").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("events");
    assert_eq!(events, 1);
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    raw.confirm_cpu_cleanup(r)
        .await
        .expect("independent declared cleanup, not HTTP cancellation");
    let terminal = cancel(&f, "cancel-running", f.headers.clone(), empty).await;
    assert_eq!(terminal.1["execution_state"], "cancelled");
    assert_eq!(terminal.1["settlement_state"], "settled");
    assert_eq!(balance(&f).await, (1000, 0));
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn cancelled_request_rolls_back_when_durable_event_cannot_commit() {
    let (f, server, c, r) = super::cleanup_cases::setup_cleanup("client-dd", true).await;
    let job = Uuid::parse_str(&f.job).expect("fixture UUID");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION cancel_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.event_type='job.cancel_requested' AND EXISTS(SELECT 1 FROM event_streams WHERE stream_id=NEW.stream_id AND entity_id='{job}') THEN RAISE EXCEPTION 'explicit cancellation event fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER cancel_fault_{suffix} BEFORE INSERT ON durable_events FOR EACH ROW EXECUTE FUNCTION cancel_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("scoped fault");
    let result = cancel(&f, "cancel-fault", f.headers.clone(), serde_json::json!({})).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER cancel_fault_{suffix} ON durable_events; DROP FUNCTION cancel_fault_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact fault");
    assert_eq!(result.0, 503);
    assert_eq!(
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("state")
            .execution_state,
        "running"
    );
    assert_eq!(balance(&f).await, (900, 100));
    assert_eq!(
        cancel(&f, "cancel-fault", f.headers.clone(), serde_json::json!({}))
            .await
            .0,
        200
    );
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    raw.confirm_cpu_cleanup(r).await.expect("fixture cleanup");
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn team_role_does_not_allow_cancelling_another_submitter_and_stop_survives_grant_loss() {
    let f = setup_model_scope(
        "client-df",
        &"b".repeat(64),
        &"c".repeat(64),
        "team cancel fixture".into(),
        32,
        true,
    )
    .await;
    let other = setup("client-dg").await;
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'admin')")
        .bind(&f.owner)
        .bind(&other.user)
        .execute(f.d.store.pool())
        .await
        .expect("other current admin fixture");
    assert_eq!(
        cancel(
            &f,
            "team-stop",
            other.headers.clone(),
            serde_json::json!({})
        )
        .await
        .0,
        403,
        "team admin is not this Job's submitter"
    );
    sqlx::query("UPDATE memberships SET role='viewer' WHERE organization_id=$1 AND user_id=$2")
        .bind(&f.owner)
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("creator downgraded");
    sqlx::query("UPDATE budget_user_grants SET can_submit=false WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("spending removed");
    assert_eq!(
        cancel(&f, "team-stop", f.headers.clone(), serde_json::json!({}))
            .await
            .0,
        200,
        "stopping original work needs no new spending permission"
    );
    assert_eq!(balance(&f).await, (1000, 0));
    f.objects.drain().await.expect("drain");
    other.objects.drain().await.expect("drain");
}
