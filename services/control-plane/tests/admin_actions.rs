//! Real PostgreSQL and HTTP handlers; explicit sessions are fixtures, not issued MFA.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use orange_control_plane::{
    admin::{
        SESSION_COOKIE, hash_secret,
        identity::{AdminIdentity, OidcConfig},
        random_secret, routes,
    },
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Actor {
    id: String,
    token: String,
    csrf: String,
}
struct Fixture {
    pool: PgPool,
    router: Router,
    origin: String,
    node: String,
    maker: Actor,
    checker: Actor,
    stranger: Actor,
}
async fn principal(pool: &PgPool, issuer: &str, scope: &str) -> Actor {
    let a = Actor {
        id: Uuid::new_v4().to_string(),
        token: random_secret(),
        csrf: random_secret(),
    };
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&a.id)
        .bind(issuer)
        .execute(pool)
        .await
        .expect("principal");
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'compute_operator','dev','resource',$2,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(&a.id).bind(scope).execute(pool).await.expect("grant");
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+600,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(hash_secret(&a.token)).bind(&a.id).bind(&a.csrf).execute(pool).await.expect("explicit session fixture");
    a
}
async fn fixture() -> Fixture {
    let store = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("real DB"))
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = store.pool().clone();
    let config: OidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("OIDC file"))
            .expect("read config"),
    )
    .expect("config");
    assert_eq!(config.environment, "dev");
    let node = Uuid::new_v4().to_string();
    let owner = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(&pool)
        .await
        .expect("owner");
    sqlx::query("INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES($1,$2,'dev','test-region','online')").bind(&node).bind(&owner).execute(&pool).await.expect("node");
    let maker = principal(&pool, &config.issuer, &node).await;
    let checker = principal(&pool, &config.issuer, &node).await;
    let stranger = principal(&pool, &config.issuer, &Uuid::new_v4().to_string()).await;
    let origin = config.admin_origin.clone();
    let identity = AdminIdentity::discover(pool.clone(), config)
        .await
        .expect("real discovery");
    Fixture {
        pool,
        router: routes::router(identity),
        origin,
        node,
        maker,
        checker,
        stranger,
    }
}
fn request(f: &Fixture, kind: &str) -> Value {
    let mut payload = json!({"node_id":f.node});
    if kind == "node.resume" {
        payload["health_report_id"] = json!("test-health-not-trusted");
    }
    if kind == "node.quarantine" {
        payload["reason_code"] = json!("maintenance");
    }
    json!({"scope":{"environment":"dev","kind":"resource","scope_id":f.node},"target_version":"v1","ticket_id":"test-ticket","reason":"Isolated local acceptance test","action_type":kind,"payload":payload})
}
async fn http(
    f: &Fixture,
    actor: &Actor,
    method: &str,
    path: &str,
    key: &str,
    etag: &str,
    body: Value,
) -> (StatusCode, Value, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("cookie", format!("{SESSION_COOKIE}={}", actor.token))
        .header("origin", &f.origin)
        .header("x-csrf-token", &actor.csrf)
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .header("if-match", etag)
        .body(Body::from(body.to_string()))
        .expect("request");
    let res = f.router.clone().oneshot(req).await.expect("handler");
    assert_eq!(res.headers()["cache-control"], "no-store");
    let tag = res
        .headers()
        .get("etag")
        .map(|v| v.to_str().expect("etag").to_owned())
        .unwrap_or_default();
    let status = res.status();
    let raw = to_bytes(res.into_body(), 128 * 1024).await.expect("body");
    (status, serde_json::from_slice(&raw).expect("JSON"), tag)
}
async fn create(f: &Fixture, kind: &str, key: &str) -> Value {
    let (status, doc, tag) = http(
        f,
        &f.maker,
        "POST",
        "/admin/v1/actions",
        key,
        "",
        request(f, kind),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    assert_eq!(tag, "\"v1\"");
    doc
}
fn path(doc: &Value, operation: &str) -> String {
    format!(
        "/admin/v1/actions/{}/{}",
        doc["action_id"].as_str().expect("id"),
        operation
    )
}
fn preview(doc: &Value) -> Value {
    json!({"payload_digest":doc["payload_digest"],"policy_version":doc["policy_version"]})
}
fn decision(doc: &Value, value: &str) -> Value {
    let mut p = preview(doc);
    p["decision"] = json!(value);
    p["reason"] = json!("Independent local review");
    p
}
async fn commands(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_node_commands WHERE node_id=$1")
        .bind(&f.node)
        .fetch_one(&f.pool)
        .await
        .expect("count")
}

#[tokio::test]
async fn drain_is_atomic_idempotent_intent_not_device_completion() {
    let f = fixture().await;
    let doc = create(&f, "node.drain", "create").await;
    assert_eq!(doc["approval_policy"], "single_operator");
    assert_eq!(doc["state"], "ready");
    let (status, replay, _) = http(
        &f,
        &f.maker,
        "POST",
        "/admin/v1/actions",
        "create",
        "",
        request(&f, "node.drain"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replay, doc);
    let (status, queued, tag) = http(
        &f,
        &f.maker,
        "POST",
        &path(&doc, "execute"),
        "execute",
        "\"v1\"",
        preview(&doc),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "executing");
    assert_eq!(tag, "\"v2\"");
    let (_, replay, _) = http(
        &f,
        &f.maker,
        "POST",
        &path(&doc, "execute"),
        "execute",
        "\"v1\"",
        preview(&doc),
    )
    .await;
    assert_eq!(replay, queued);
    assert_eq!(commands(&f).await, 1);
    let state: String = sqlx::query_scalar("SELECT state FROM nodes WHERE node_id=$1")
        .bind(&f.node)
        .fetch_one(&f.pool)
        .await
        .expect("state");
    assert_eq!(state, "draining");
    let generation: i64 =
        sqlx::query_scalar("SELECT policy_generation FROM nodes WHERE node_id=$1")
            .bind(&f.node)
            .fetch_one(&f.pool)
            .await
            .expect("policy generation");
    assert_eq!(generation, 1);
    let queued_generation: i64 =
        sqlx::query_scalar("SELECT policy_generation FROM admin_node_commands WHERE node_id=$1")
            .bind(&f.node)
            .fetch_one(&f.pool)
            .await
            .expect("immutable command generation");
    assert_eq!(queued_generation, generation);
    let node_path = format!("/admin/v1/nodes/{}?environment=dev", f.node);
    let (status, node, tag) = http(&f, &f.maker, "GET", &node_path, "", "", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["status"], "draining");
    assert_eq!(tag, "\"v2\"");
    assert_eq!(
        http(&f, &f.stranger, "GET", &node_path, "", "", json!(null))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "cancel"),
            "cancel",
            "\"v2\"",
            json!({"reason":"Try unsafe cancellation"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_action_events WHERE action_id=$1 AND kind='action.execute'",
    )
    .bind(doc["action_id"].as_str().expect("id"))
    .fetch_one(&f.pool)
    .await
    .expect("events");
    assert_eq!(n, 1);
    let list = format!(
        "/admin/v1/actions?environment=dev&scope_kind=resource&scope_id={}",
        f.node
    );
    let (status, listed, _) = http(&f, &f.maker, "GET", &list, "", "", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().expect("list").len(), 1);
}
#[tokio::test]
async fn maker_checker_requires_distinct_current_principals_and_trusted_health() {
    let f = fixture().await;
    let doc = create(&f, "node.resume", "create").await;
    assert_eq!(doc["required_approvals"], 1);
    assert_eq!(doc["state"], "pending_approval");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "approvals"),
            "self",
            "\"v1\"",
            decision(&doc, "approve")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http(
            &f,
            &f.stranger,
            "POST",
            &path(&doc, "approvals"),
            "other",
            "\"v1\"",
            decision(&doc, "approve")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut wrong = decision(&doc, "approve");
    wrong["payload_digest"] = json!("a".repeat(64));
    assert_eq!(
        http(
            &f,
            &f.checker,
            "POST",
            &path(&doc, "approvals"),
            "bad",
            "\"v1\"",
            wrong
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, approved, _) = http(
        &f,
        &f.checker,
        "POST",
        &path(&doc, "approvals"),
        "approve",
        "\"v1\"",
        decision(&doc, "approve"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approved["state"], "ready");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "execute"),
            "execute",
            "\"v2\"",
            preview(&doc)
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(commands(&f).await, 0);
    sqlx::query("UPDATE admin_grants SET revoked=TRUE WHERE principal_id=$1")
        .bind(&f.checker.id)
        .execute(&f.pool)
        .await
        .expect("revoke");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "execute"),
            "execute",
            "\"v2\"",
            preview(&doc)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, current, _) = http(
        &f,
        &f.maker,
        "GET",
        path(&doc, "").trim_end_matches('/'),
        "",
        "",
        json!(null),
    )
    .await;
    assert_eq!(current["state"], "ready");
    assert_eq!(current["version"], "v2");
}
#[tokio::test]
async fn rejection_cancellation_expiry_and_stale_previews_have_no_commands() {
    let f = fixture().await;
    let doc = create(&f, "node.resume", "reject-create").await;
    let (status, rejected, _) = http(
        &f,
        &f.checker,
        "POST",
        &path(&doc, "approvals"),
        "reject",
        "\"v1\"",
        decision(&doc, "reject"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rejected["state"], "rejected");
    assert_eq!(
        http(
            &f,
            &f.checker,
            "POST",
            &path(&doc, "approvals"),
            "approve",
            "\"v2\"",
            decision(&doc, "approve")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let drain = create(&f, "node.drain", "drain").await;
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&drain, "execute"),
            "stale",
            "\"v9\"",
            preview(&drain)
        )
        .await
        .0,
        StatusCode::PRECONDITION_FAILED
    );
    let (status, cancelled, _) = http(
        &f,
        &f.maker,
        "POST",
        &path(&drain, "cancel"),
        "cancel",
        "\"v1\"",
        json!({"reason":"Withdraw before dispatch"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&drain, "execute"),
            "late",
            "\"v2\"",
            preview(&drain)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let expired = create(&f, "node.drain", "expiry").await;
    sqlx::query("UPDATE admin_action_control SET expires_at=1 WHERE action_id=$1")
        .bind(expired["action_id"].as_str().expect("id"))
        .execute(&f.pool)
        .await
        .expect("time fixture");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&expired, "execute"),
            "expired",
            "\"v1\"",
            preview(&expired)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(commands(&f).await, 0);
}
#[tokio::test]
async fn concurrent_target_versions_and_duplicate_dispatch_are_serialized() {
    let f = fixture().await;
    let a = create(&f, "node.drain", "a").await;
    let b = create(&f, "node.quarantine", "b").await;
    let pa = path(&a, "execute");
    let pb = path(&b, "execute");
    let (ra, rb) = tokio::join!(
        http(&f, &f.maker, "POST", &pa, "ea", "\"v1\"", preview(&a)),
        http(&f, &f.checker, "POST", &pb, "eb", "\"v1\"", preview(&b))
    );
    assert!(
        (ra.0 == StatusCode::ACCEPTED && rb.0 == StatusCode::PRECONDITION_FAILED)
            || (rb.0 == StatusCode::ACCEPTED && ra.0 == StatusCode::PRECONDITION_FAILED),
        "{:?} {:?}",
        ra.0,
        rb.0
    );
    assert_eq!(commands(&f).await, 1);
    let (doc, p, key) = if ra.0 == StatusCode::ACCEPTED {
        (&a, &pa, "ea")
    } else {
        (&b, &pb, "eb")
    };
    let actor = if key == "ea" { &f.maker } else { &f.checker };
    let (r1, r2) = tokio::join!(
        http(&f, actor, "POST", p, key, "\"v1\"", preview(doc)),
        http(&f, actor, "POST", p, key, "\"v1\"", preview(doc))
    );
    assert_eq!(r1.0, StatusCode::ACCEPTED);
    assert_eq!(r1.1, r2.1);
    assert_eq!(commands(&f).await, 1);
}
#[tokio::test]
async fn schema_scope_digest_and_idempotency_mismatch_are_rejected() {
    let f = fixture().await;
    let doc = create(&f, "node.drain", "same-key").await;
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            "/admin/v1/actions",
            "same-key",
            "",
            request(&f, "node.quarantine")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut bad = request(&f, "node.drain");
    bad["payload"]["health_report_id"] = json!(null);
    assert_eq!(
        http(&f, &f.maker, "POST", "/admin/v1/actions", "null", "", bad)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut bad = request(&f, "node.drain");
    bad["approved"] = json!(true);
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            "/admin/v1/actions",
            "unknown",
            "",
            bad
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut bad = request(&f, "node.drain");
    bad["payload"]["node_id"] = json!(Uuid::new_v4().to_string());
    assert_eq!(
        http(&f, &f.maker, "POST", "/admin/v1/actions", "scope", "", bad)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let mut bad = preview(&doc);
    bad["policy_version"] = json!("outdated");
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "execute"),
            "policy",
            "\"v1\"",
            bad
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let get = path(&doc, "");
    assert_eq!(
        http(
            &f,
            &f.stranger,
            "GET",
            get.trim_end_matches('/'),
            "",
            "",
            json!(null)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(commands(&f).await, 0);
}
#[tokio::test]
async fn live_revocation_step_up_and_csrf_apply_to_retries() {
    let f = fixture().await;
    let doc = create(&f, "node.drain", "create").await;
    let bad = Actor {
        id: f.maker.id.clone(),
        token: f.maker.token.clone(),
        csrf: "wrong".into(),
    };
    assert_eq!(
        http(
            &f,
            &bad,
            "POST",
            &path(&doc, "execute"),
            "csrf",
            "\"v1\"",
            preview(&doc)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE admin_sessions SET auth_time=1 WHERE principal_id=$1")
        .bind(&f.maker.id)
        .execute(&f.pool)
        .await
        .expect("old MFA");
    let (status, error, _) = http(
        &f,
        &f.maker,
        "POST",
        &path(&doc, "execute"),
        "step-up",
        "\"v1\"",
        preview(&doc),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["code"], "STEP_UP_REQUIRED");
    sqlx::query("UPDATE admin_sessions SET auth_time=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint WHERE principal_id=$1").bind(&f.maker.id).execute(&f.pool).await.expect("fresh fixture");
    sqlx::query("UPDATE admin_grants SET revoked=TRUE WHERE principal_id=$1")
        .bind(&f.maker.id)
        .execute(&f.pool)
        .await
        .expect("revoke maker");
    assert_eq!(
        http(
            &f,
            &f.checker,
            "POST",
            &path(&doc, "execute"),
            "maker-revoked",
            "\"v1\"",
            preview(&doc)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            "/admin/v1/actions",
            "create",
            "",
            request(&f, "node.drain")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(commands(&f).await, 0);
}
#[tokio::test]
async fn database_protects_request_and_event_evidence() {
    let f = fixture().await;
    let doc = create(&f, "node.drain", "create").await;
    let action = doc["action_id"].as_str().expect("id");
    assert!(sqlx::query("UPDATE admin_actions SET document=jsonb_set(document,'{payload_digest}',to_jsonb(repeat('a',64))) WHERE action_id=$1").bind(action).execute(&f.pool).await.is_err());
    assert!(
        sqlx::query("DELETE FROM admin_action_events WHERE action_id=$1")
            .bind(action)
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM admin_action_keys WHERE action_id=$1")
            .bind(action)
            .execute(&f.pool)
            .await
            .is_err()
    );
    let (_, unchanged, _) = http(
        &f,
        &f.maker,
        "GET",
        path(&doc, "").trim_end_matches('/'),
        "",
        "",
        json!(null),
    )
    .await;
    assert_eq!(unchanged, doc);
}
