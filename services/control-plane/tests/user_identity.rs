//! Actual DB/session/organization boundary with explicit session fixtures. Real login is a separate browser test.
#[path = "user_identity/accounts.rs"]
mod account_cases;
#[cfg(feature = "cloud-host-tests")]
#[path = "user_identity/cloud_runs.rs"]
mod cloud_host_tests;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use orange_control_plane::{
    auth::{
        self, AuthError, hash_secret, random_secret,
        users::{USER_COOKIE, UserIdentity, UserOidcConfig},
    },
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
#[cfg(all(unix, feature = "confluence-fixture-tests"))]
#[path = "user_identity/confluence_consent.rs"]
mod confluence_consent;
#[cfg(all(
    unix,
    any(
        feature = "pg-view-tests",
        feature = "s3-source-tests",
        feature = "repository-fixture-tests",
        feature = "notion-fixture-tests",
        feature = "confluence-fixture-tests"
    )
))]
#[path = "user_identity/connector_run.rs"]
mod connector_run;
#[cfg(all(unix, feature = "notion-fixture-tests"))]
#[path = "user_identity/notion_consent.rs"]
mod notion_consent;
#[cfg(all(unix, feature = "pg-view-tests"))]
#[path = "user_identity/pg_view_consent.rs"]
mod pg_consent;
#[cfg(all(unix, feature = "repository-fixture-tests"))]
#[path = "user_identity/repository_consent.rs"]
mod repository_consent;
#[cfg(all(unix, feature = "s3-source-tests"))]
#[path = "user_identity/s3_consent.rs"]
mod s3_consent;
#[path = "user_identity/team_operations.rs"]
mod team_operations;
struct Fixture {
    identity: UserIdentity,
    router: Router,
    pool: PgPool,
    user: String,
    token: String,
    csrf: String,
}
async fn fixture() -> Fixture {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("actual database required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    fixture_from_store(&store).await
}
async fn fixture_from_store(store: &Store) -> Fixture {
    let config: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("real provider config required"),
        )
        .expect("config file"),
    )
    .expect("config");
    assert_eq!(config.environment, "dev");
    let pool = store.pool().clone();
    let user = Uuid::new_v4().to_string();
    let token = random_secret();
    let csrf = random_secret();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&user)
        .execute(&pool)
        .await
        .expect("owner fixture");
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
        .bind(&user)
        .execute(&pool)
        .await
        .expect("zero wallet fixture");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,$2,$1,$1)")
        .bind(&user)
        .bind(&config.issuer)
        .execute(&pool)
        .await
        .expect("identity fixture");
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)")
        .bind(hash_secret(&token)).bind(Uuid::new_v4().to_string()).bind(&user).bind(&csrf).execute(&pool).await.expect("session fixture, not signed-token evidence");
    let identity = UserIdentity::discover(pool.clone(), config)
        .await
        .expect("actual provider discovery");
    let router = auth::routes::router(identity.clone());
    Fixture {
        identity,
        router,
        pool,
        user,
        token,
        csrf,
    }
}
fn headers(f: &Fixture) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        format!("{USER_COOKIE}={}", f.token)
            .parse()
            .expect("cookie"),
    );
    headers.insert("origin", "https://127.0.0.1:43122".parse().expect("origin"));
    headers.insert("x-csrf-token", f.csrf.parse().expect("csrf"));
    headers.insert("idempotency-key", "fixture-intent".parse().expect("key"));
    headers
}
async fn request(
    f: &Fixture,
    method: &str,
    path: &str,
    body: Option<Value>,
    header: HeaderMap,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .uri(path)
        .method(method)
        .header("content-type", "application/json");
    *request.headers_mut().expect("headers") = header;
    request = request.header("content-type", "application/json");
    let response = f
        .router
        .clone()
        .oneshot(
            request
                .body(
                    body.map(|v| Body::from(v.to_string()))
                        .unwrap_or_else(Body::empty),
                )
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.expect("body");
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("JSON")
        },
    )
}
#[tokio::test]
async fn user_api_requires_separate_cookie_current_actor_and_csrf_with_idempotent_organization_creation()
 {
    let f = fixture().await;
    for cookie in [None, Some(format!("__Host-orange-admin={}", f.token))] {
        let mut h = HeaderMap::new();
        if let Some(cookie) = cookie {
            h.insert("cookie", cookie.parse().expect("header"));
        }
        assert_eq!(
            request(&f, "GET", "/api/v1/session", None, h).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, session) = request(&f, "GET", "/api/v1/session", None, headers(&f)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["user_id"], f.user);
    let body = json!({"name":"User API fixture","member_limit":20});
    let mut missing = headers(&f);
    missing.remove("x-csrf-token");
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/organizations",
            Some(body.clone()),
            missing
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut wrong = headers(&f);
    wrong.insert(
        "origin",
        "https://127.0.0.1:43112".parse().expect("other origin"),
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/organizations",
            Some(body.clone()),
            wrong
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/organizations",
            Some(json!({"name":"Spoof","member_limit":20,"actor_id":"another"})),
            headers(&f)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, created) = request(
        &f,
        "POST",
        "/api/v1/organizations",
        Some(body.clone()),
        headers(&f),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["member_user_id"], f.user);
    assert_eq!(created["role"], "owner");
    assert!(created["policy_version"].is_string());
    let repeated = request(&f, "POST", "/api/v1/organizations", Some(body), headers(&f)).await;
    assert_eq!(repeated.0, StatusCode::OK);
    assert_eq!(repeated.1["organization_id"], created["organization_id"]);
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/organizations",
            Some(json!({"name":"Changed","member_limit":20})),
            headers(&f)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let other = fixture().await;
    let path = format!(
        "/api/v1/organizations/{}",
        created["organization_id"].as_str().expect("organization")
    );
    assert_eq!(
        request(&other, "GET", &path, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, list) = request(&f, "GET", "/api/v1/organizations", None, headers(&f)).await;
    assert_eq!(list["user_id"], f.user);
    assert_eq!(list["data"].as_array().expect("array").len(), 1);
    assert_eq!(
        request(&f, "GET", "/admin/v1/session", None, headers(&f))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn disabled_user_can_revoke_own_session_and_reactivation_does_not_restore_it() {
    let f = fixture().await;
    sqlx::query("UPDATE users SET active=FALSE WHERE user_id=$1")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("disable fixture");
    assert!(matches!(
        f.identity.authenticate(&headers(&f)).await,
        Err(AuthError::Unauthenticated)
    ));
    f.identity
        .revoke_self(&headers(&f))
        .await
        .expect("self logout after revocation");
    f.identity
        .revoke_self(&headers(&f))
        .await
        .expect("idempotent logout");
    sqlx::query("UPDATE users SET active=TRUE WHERE user_id=$1")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("reactivate fixture");
    assert!(matches!(
        f.identity.authenticate(&headers(&f)).await,
        Err(AuthError::Unauthenticated)
    ));
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_auth_audit WHERE user_id=$1 AND event_type='session.revoked'",
    )
    .bind(&f.user)
    .fetch_one(&f.pool)
    .await
    .expect("one revoke fact");
    assert_eq!(events, 1);
}
#[tokio::test]
async fn session_expiry_while_domain_operation_waits_rolls_back_the_entire_creation() {
    let f = fixture().await;
    let mut blocker = f.pool.begin().await.expect("blocking fixture");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("orange.team.create:{}", f.user))
        .execute(&mut *blocker)
        .await
        .expect("hold create-key lock");
    sqlx::query("UPDATE user_sessions SET idle_expires_at=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3,absolute_expires_at=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3 WHERE user_id=$1").bind(&f.user).execute(&f.pool).await.expect("short actual expiry");
    let app = f.router.clone();
    let mut req = Request::builder()
        .uri("/api/v1/organizations")
        .method("POST");
    *req.headers_mut().expect("headers") = headers(&f);
    let request = req
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name":"Must roll back","member_limit":20}).to_string(),
        ))
        .expect("request");
    let pending = tokio::spawn(async move { app.oneshot(request).await });
    let mut waiting = false;
    for _ in 0..20 {
        waiting=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event='advisory' AND query LIKE '%hashtextextended%')").fetch_one(&f.pool).await.expect("observe authenticated transaction waiting");
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        waiting,
        "request did not reach the domain lock while authorized"
    );
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    blocker.commit().await.expect("release domain lock");
    let response = pending.await.expect("task").expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id=$1")
        .bind(&f.user)
        .fetch_one(&f.pool)
        .await
        .expect("no partial organization");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn revoked_session_cannot_authenticate_and_owner_ledger_remains_balanced_and_immutable() {
    let f = fixture().await;
    let owner: String = sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1")
        .bind(&f.user)
        .fetch_one(&f.pool)
        .await
        .expect("actor owner");
    let id = Uuid::new_v4().to_string();
    let mut tx = f.pool.begin().await.expect("fixture transaction");
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&id)
    .bind(format!("fixture-retention-{id}"))
    .bind(&owner)
    .execute(&mut *tx)
    .await
    .expect("retention fixture");
    sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES ($1,'retention_fixture_a',17),($1,'retention_fixture_b',-17)").bind(&id).execute(&mut *tx).await.expect("balanced development postings");
    tx.commit().await.expect("commit");
    assert!(f.identity.authenticate(&headers(&f)).await.is_ok());
    f.identity.revoke_self(&headers(&f)).await.expect("logout");
    assert!(matches!(
        f.identity.authenticate(&headers(&f)).await,
        Err(AuthError::Unauthenticated)
    ));
    // Physically expire only this test user's revoked session before looking up the financial facts.
    let removed = sqlx::query("DELETE FROM user_sessions WHERE user_id=$1 AND revoked")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("remove revoked fixture session");
    assert_eq!(removed.rows_affected(), 1);
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_auth_audit WHERE user_id=$1 AND event_type='session.revoked'",
    )
    .bind(&f.user)
    .fetch_one(&f.pool)
    .await
    .expect("retained audit");
    assert_eq!(audit, 1);
    let rows:Vec<(String,String,i64)>=sqlx::query_as("SELECT t.owner_id,e.account,e.amount FROM ledger_entries e JOIN ledger_transactions t USING(transaction_id) WHERE e.transaction_id=$1 ORDER BY e.account")
        .bind(&id).fetch_all(&f.pool).await.expect("retained facts");
    assert_eq!(
        rows,
        vec![
            (owner.clone(), "retention_fixture_a".into(), 17),
            (owner, "retention_fixture_b".into(), -17)
        ]
    );
    assert!(
        sqlx::query("DELETE FROM ledger_transactions WHERE transaction_id=$1")
            .bind(&id)
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE ledger_entries SET amount=0 WHERE transaction_id=$1")
            .bind(&id)
            .execute(&f.pool)
            .await
            .is_err()
    );
}
