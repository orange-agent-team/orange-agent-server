//! Actual PostgreSQL authorization/expiry tests with explicit session fixtures.
//! These do not prove MFA issuance; the separate Chrome/Keycloak tests prove that boundary.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
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

struct Fixture {
    identity: AdminIdentity,
    router: Router,
    pool: PgPool,
    principal: String,
    token: String,
    a: String,
    b: String,
}
async fn fixture() -> Fixture {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("actual database required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let config: OidcConfig = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("real discovery config required"),
        )
        .expect("private config"),
    )
    .expect("config");
    assert_eq!(config.environment, "dev");
    let pool = store.pool().clone();
    let principal = Uuid::new_v4().to_string();
    let a = Uuid::new_v4().to_string();
    let b = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&principal)
        .bind(&config.issuer)
        .execute(&pool)
        .await
        .expect("principal fixture");
    for (role, scope) in [("compute_operator", &a), ("finance_operator", &b)] {
        sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,$2,'dev','tenant',$3,EXTRACT(EPOCH FROM clock_timestamp())::bigint+3600)").bind(&principal).bind(role).bind(scope).execute(&pool).await.expect("grant fixture");
    }
    let token = random_secret();
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,EXTRACT(EPOCH FROM clock_timestamp())::bigint,EXTRACT(EPOCH FROM clock_timestamp())::bigint+300,EXTRACT(EPOCH FROM clock_timestamp())::bigint+600)").bind(hash_secret(&token)).bind(&principal).bind(random_secret()).execute(&pool).await.expect("session fixture, not issued MFA");
    let identity = AdminIdentity::discover(pool.clone(), config)
        .await
        .expect("actual OIDC discovery");
    let router = routes::router(identity.clone());
    Fixture {
        identity,
        router,
        pool,
        principal,
        token,
        a,
        b,
    }
}
fn headers(f: &Fixture) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        format!("{SESSION_COOKIE}={}", f.token)
            .parse()
            .expect("header"),
    );
    headers
}
async fn get(f: &Fixture, path: &str) -> (StatusCode, Value) {
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("cookie", format!("{SESSION_COOKIE}={}", f.token))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (status, serde_json::from_slice(&body).expect("JSON"))
}
fn actions(scope: &str) -> String {
    format!("/admin/v1/actions?environment=dev&scope_kind=tenant&scope_id={scope}")
}
async fn action(f: &Fixture, scope: &str, kind: &str) -> String {
    let id = Uuid::new_v4().to_string();
    let payload = if kind == "node.drain" {
        json!({"node_id":"fixture-node"})
    } else {
        json!({"payment_order_id":"fixture-payment","amount":{"currency":"USD","minor_units":"1"},"reconciliation_case_id":"fixture-case"})
    };
    let doc = json!({"action_id":id,"requested_by":f.principal,"request":{"scope":{"environment":"dev","kind":"tenant","scope_id":scope},"action_type":kind,"target_version":"v1","reason":"Local authorization fixture only","ticket_id":"fixture-ticket","payload":payload},"payload_digest":"a".repeat(64),"policy_version":"v1","approval_policy":"maker_checker","required_approvals":1,"approvals":[],"state":"pending_approval","version":"v1","created_at":"2026-11-04T00:00:00Z","expires_at":"2026-11-05T00:00:00Z"});
    sqlx::query("INSERT INTO admin_actions(action_id,environment,scope_kind,scope_id,action_type,document) VALUES($1,'dev','tenant',$2,$3,$4::jsonb)").bind(&id).bind(scope).bind(kind).bind(doc.to_string()).execute(&f.pool).await.expect("read projection fixture");
    id
}

#[tokio::test]
async fn per_scope_roles_do_not_cross_multiply_and_cursor_cannot_cross_scope() {
    let f = fixture().await;
    let session = f
        .identity
        .current(&headers(&f))
        .await
        .expect("fixture session");
    assert!(!session.view.permissions.contains(&"nodes.read".to_owned()));
    assert!(
        !session
            .view
            .permissions
            .contains(&"payments.read".to_owned())
    );
    action(&f, &f.a, "node.drain").await;
    action(&f, &f.a, "node.drain").await;
    action(&f, &f.a, "payment.refund").await;
    action(&f, &f.b, "node.drain").await;
    let payment = action(&f, &f.b, "payment.refund").await;
    let (status, first) = get(&f, &(actions(&f.a) + "&limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"].as_array().expect("array").len(), 1);
    assert_eq!(first["data"][0]["request"]["action_type"], "node.drain");
    let cursor = first["next_cursor"].as_str().expect("cursor");
    assert_eq!(
        get(&f, &(actions(&f.b) + "&cursor=" + cursor)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, second) = get(&f, &(actions(&f.a) + "&cursor=" + cursor)).await;
    assert_ne!(
        first["data"][0]["action_id"],
        second["data"][0]["action_id"]
    );
    assert!(second.get("next_cursor").is_none());
    let (_, finance) = get(&f, &actions(&f.b)).await;
    assert_eq!(finance["data"].as_array().expect("array").len(), 1);
    assert_eq!(finance["data"][0]["action_id"], payment);
    sqlx::query("UPDATE admin_grants SET revoked=TRUE WHERE principal_id=$1 AND scope_id=$2")
        .bind(&f.principal)
        .bind(&f.a)
        .execute(&f.pool)
        .await
        .expect("revoke");
    assert_eq!(get(&f, &actions(&f.a)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(get(&f, &actions(&f.b)).await.0, StatusCode::OK);
    assert!(
        sqlx::query("DELETE FROM admin_audit WHERE principal_id=$1")
            .bind(&f.principal)
            .execute(&f.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn expired_session_inactive_principal_and_expired_grants_fail_closed() {
    let f = fixture().await;
    sqlx::query("UPDATE admin_sessions SET idle_expires_at=0 WHERE token_hash=$1")
        .bind(hash_secret(&f.token))
        .execute(&f.pool)
        .await
        .expect("expire");
    assert_eq!(
        get(&f, "/admin/v1/session").await.0,
        StatusCode::UNAUTHORIZED
    );
    let f = fixture().await;
    sqlx::query("UPDATE admin_principals SET active=FALSE WHERE principal_id=$1")
        .bind(&f.principal)
        .execute(&f.pool)
        .await
        .expect("disable");
    assert_eq!(
        get(&f, "/admin/v1/session").await.0,
        StatusCode::UNAUTHORIZED
    );
    let f = fixture().await;
    sqlx::query("UPDATE admin_grants SET expires_at=0 WHERE principal_id=$1")
        .bind(&f.principal)
        .execute(&f.pool)
        .await
        .expect("expire grants");
    assert_eq!(get(&f, "/admin/v1/session").await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_origin_or_csrf_cannot_revoke_session() {
    let f = fixture().await;
    let session = f
        .identity
        .current(&headers(&f))
        .await
        .expect("fixture session");
    for (origin, csrf) in [
        ("https://attacker.invalid", session.view.csrf_token.as_str()),
        ("https://127.0.0.1:43112", "wrong"),
    ] {
        let mut request = headers(&f);
        request.insert("origin", origin.parse().expect("origin"));
        request.insert("x-csrf-token", csrf.parse().expect("csrf"));
        request.insert("idempotency-key", "fixture-key".parse().expect("key"));
        assert!(f.identity.revoke(&request, &session).await.is_err());
        assert!(f.identity.current(&headers(&f)).await.is_ok());
    }
}

#[path = "fixtures/session_deadlines.rs"]
mod session_deadlines;

#[tokio::test]
async fn session_refresh_after_idle_deadline_is_denied() {
    let f = fixture().await;
    let hash = hash_secret(&f.token);
    let deadline: i64 = sqlx::query_scalar("UPDATE admin_sessions SET idle_expires_at=floor(extract(epoch FROM clock_timestamp()))::bigint+4 WHERE token_hash=$1 RETURNING idle_expires_at")
        .bind(&hash).fetch_one(&f.pool).await.expect("brief admin session");
    let mut blocker = f.pool.begin().await.expect("fixture transaction");
    sqlx::query("SELECT 1 FROM admin_sessions WHERE token_hash=$1 FOR UPDATE")
        .bind(&hash)
        .fetch_one(&mut *blocker)
        .await
        .expect("lock test session");
    let (response, ()) = tokio::join!(
        get(&f, "/admin/v1/session"),
        session_deadlines::release_at_deadline(&f.pool, blocker, deadline)
    );
    assert_eq!(response.0, StatusCode::UNAUTHORIZED);
    assert!(response.1.get("csrf_token").is_none());
    let stored: i64 =
        sqlx::query_scalar("SELECT idle_expires_at FROM admin_sessions WHERE token_hash=$1")
            .bind(hash)
            .fetch_one(&f.pool)
            .await
            .expect("read deadline");
    assert_eq!(stored, deadline);
}

#[tokio::test]
async fn session_refresh_keeps_only_grants_still_live_after_waiting() {
    let f = fixture().await;
    let deadline: i64 = sqlx::query_scalar("UPDATE admin_grants SET expires_at=floor(extract(epoch FROM clock_timestamp()))::bigint+4 WHERE principal_id=$1 AND role='finance_operator' RETURNING expires_at")
        .bind(&f.principal).fetch_one(&f.pool).await.expect("brief finance grant");
    let mut blocker = f.pool.begin().await.expect("fixture transaction");
    sqlx::query(
        "SELECT 1 FROM admin_grants WHERE principal_id=$1 AND role='finance_operator' FOR UPDATE",
    )
    .bind(&f.principal)
    .fetch_one(&mut *blocker)
    .await
    .expect("lock test grant");
    let (response, ()) = tokio::join!(
        get(&f, "/admin/v1/session"),
        session_deadlines::release_at_deadline(&f.pool, blocker, deadline)
    );
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["roles"], json!(["compute_operator"]));
    assert_eq!(response.1["scopes"].as_array().expect("scopes").len(), 1);
    assert_eq!(response.1["scopes"][0]["scope_id"], f.a);
}

#[cfg(feature = "issuer-directory-tests")]
#[path = "admin_identity/issuer_directory.rs"]
mod issuer_directory;
