//! Real PG migrations and HTTP authorization; sessions are explicit fixtures.
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
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn session(pool: &PgPool, principal: &str) -> String {
    let token = random_secret();
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+600,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)")
        .bind(hash_secret(&token)).bind(principal).bind(random_secret()).execute(pool).await.expect("explicit session fixture");
    token
}
async fn actor(pool: &PgPool, issuer: &str, kind: &str, scope: &str) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&id)
        .bind(issuer)
        .execute(pool)
        .await
        .expect("principal");
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'auditor','dev',$2,$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(&id).bind(kind).bind(scope).execute(pool).await.expect("scoped auditor");
    let token = session(pool, &id).await;
    (id, token)
}
async fn seed(pool: &PgPool, id: &str, env: Option<&str>, scope: Option<&str>) {
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,occurred_at) VALUES($1,'fixture-actor','fixture.recorded',$2,$3,$4,1700000000)")
        .bind(id).bind(env).bind(scope.map(|_|"tenant")).bind(scope).execute(pool).await.expect("original immutable fixture");
}
async fn get(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(path)
        .header("cookie", format!("{SESSION_COOKIE}={token}"))
        .header("x-request-id", "audit-test-request")
        .body(Body::empty())
        .expect("request");
    let res = app
        .clone()
        .oneshot(request)
        .await
        .expect("real HTTP handler");
    assert_eq!(res.headers()["cache-control"], "no-store");
    let status = res.status();
    if status == StatusCode::OK {
        assert_eq!(res.headers()["x-request-id"], "audit-test-request");
    }
    let value = serde_json::from_slice(
        &to_bytes(res.into_body(), 512 * 1024)
            .await
            .expect("bounded response"),
    )
    .expect("JSON");
    (status, value)
}
fn list(scope: &str, limit: u16) -> String {
    format!(
        "/admin/v1/audit-events?environment=dev&scope_kind=tenant&scope_id={scope}&limit={limit}"
    )
}
fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .expect("list")
        .iter()
        .map(|r| r["event_id"].as_str().expect("id").to_owned())
        .collect()
}

#[tokio::test]
async fn migrated_audit_scope_cursor_snapshot_trace_and_revocation() {
    let store = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("dedicated DB"))
        .await
        .expect("connect");
    let pool = store.pool().clone();
    let expected =
        std::env::var("ORANGE_AUDIT_TEST_DATABASE").expect("explicit isolated database marker");
    let actual: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("database identity");
    assert!(
        expected.starts_with("orange_audit_test_") && actual == expected,
        "never migrate or seed an unowned shared database"
    );
    let migrations = sqlx::migrate!();
    sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .filter(|m| m.version < 30)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await
    .expect("real prior29 migrations");
    let a = Uuid::new_v4().to_string();
    let b = Uuid::new_v4().to_string();
    seed(&pool, "legacy-a", Some("dev"), Some(&a)).await;
    seed(&pool, "foreign-b", Some("dev"), Some(&b)).await;
    seed(&pool, "foreign-production", Some("production"), Some(&a)).await;
    seed(&pool, "unscoped-session", None, None).await;
    let original: Value =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM admin_audit a WHERE event_id='legacy-a'")
            .fetch_one(&pool)
            .await
            .expect("legacy evidence");
    store.migrate().await.expect("actual additive migration30");
    let retained:Value=sqlx::query_scalar("SELECT to_jsonb(a)-'audit_sequence'-'request_id' FROM admin_audit a WHERE event_id='legacy-a'").fetch_one(&pool).await.expect("retained original");
    assert_eq!(
        retained, original,
        "migration must not invent or replace old audit fields"
    );
    assert!(
        sqlx::query("UPDATE admin_audit SET event_type='changed' WHERE event_id='legacy-a'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM admin_audit WHERE event_id='legacy-a'")
            .execute(&pool)
            .await
            .is_err()
    );
    for i in 0..4 {
        seed(&pool, &format!("new-a-{i}"), Some("dev"), Some(&a)).await;
    }
    let config: OidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("config path"))
            .expect("private config"),
    )
    .expect("config");
    let (principal, token) = actor(&pool, &config.issuer, "tenant", &a).await;
    let (_, foreign) = actor(&pool, &config.issuer, "tenant", &b).await;
    let (_, platform) = actor(&pool, &config.issuer, "platform", "platform").await;
    let identity = AdminIdentity::discover(pool.clone(), config)
        .await
        .expect("actual discovery;session issuance is a fixture");
    let app = routes::router(identity);
    let (status, first) = get(&app, &token, &list(&a, 2)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(ids(&first), ["new-a-3", "new-a-2"]);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("opaque cursor")
        .to_owned();
    assert!(!cursor.contains(&principal));
    assert!(!cursor.contains(&a));
    let persisted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_audit_cursors WHERE cursor_hash=$1 AND session_hash=$2",
    )
    .bind(hash_secret(&cursor))
    .bind(hash_secret(&token))
    .fetch_one(&pool)
    .await
    .expect("cursor persisted only by digest");
    assert_eq!(persisted, 1);
    seed(&pool, "inserted-after-boundary", Some("dev"), Some(&a)).await;
    let page2 = format!("{}&cursor={cursor}", list(&a, 2));
    let (status, second) = get(&app, &token, &page2).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&second), ["new-a-1", "new-a-0"]);
    let (_, again) = get(&app, &token, &page2).await;
    assert_eq!(
        second["data"], again["data"],
        "read-generated events must not enter the older bounded page"
    );
    let last = second["next_cursor"].as_str().expect("last cursor");
    let (status, end) = get(&app, &token, &format!("{}&cursor={last}", list(&a, 2))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&end), ["legacy-a"]);
    assert!(end.get("next_cursor").is_none());
    assert!(end["data"][0]["request_id"].is_null());
    assert_eq!(end["data"][0]["result"], "recorded");
    let (_, fresh) = get(&app, &token, &list(&a, 200)).await;
    assert!(ids(&fresh).contains(&"inserted-after-boundary".into()));
    assert!(!ids(&fresh).contains(&"foreign-b".into()));
    assert!(
        fresh["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|r| r["event_type"] == "audit.list" && r["request_id"] == "audit-test-request")
    );
    let (status, all) = get(
        &app,
        &platform,
        "/admin/v1/audit-events?environment=dev&scope_kind=platform&scope_id=platform&limit=200",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&all).contains(&"foreign-b".into()));
    assert!(!ids(&all).contains(&"foreign-production".into()));
    assert!(!ids(&all).contains(&"unscoped-session".into()));
    let (status, detail) = get(
        &app,
        &token,
        "/admin/v1/audit-events/legacy-a?environment=dev",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["event_id"], "legacy-a");
    assert!(detail["request_id"].is_null());
    for (who, path, expected) in [
        (
            &token,
            format!("{}&cursor={cursor}", list(&b, 2)),
            StatusCode::FORBIDDEN,
        ),
        (
            &foreign,
            format!("{}&cursor={cursor}", list(&b, 2)),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            &token,
            "/admin/v1/audit-events/foreign-b?environment=dev".into(),
            StatusCode::NOT_FOUND,
        ),
        (
            &token,
            "/admin/v1/audit-events/foreign-production?environment=dev".into(),
            StatusCode::NOT_FOUND,
        ),
        (
            &platform,
            "/admin/v1/audit-events/unscoped-session?environment=dev".into(),
            StatusCode::NOT_FOUND,
        ),
        (
            &token,
            format!("{}&cursor=not-an-issued-cursor", list(&a, 2)),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            &token,
            format!("{}&dangerous=true", list(&a, 2)),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (&token, list(&a, 201), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let (status, body) = get(&app, who, &path).await;
        assert_eq!(status, expected, "{body}");
        assert!(body.get("data").is_none());
    }
    let second_login = session(&pool, &principal).await;
    assert_eq!(
        get(&app, &second_login, &page2).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "cursor cannot cross sessions of the same principal"
    );
    let expired = random_secret();
    sqlx::query("INSERT INTO admin_audit_cursors(cursor_hash,session_hash,principal_id,authority,snapshot_sequence,before_sequence,expires_at) SELECT $1,session_hash,principal_id,authority,snapshot_sequence,before_sequence,1 FROM admin_audit_cursors WHERE cursor_hash=$2")
        .bind(hash_secret(&expired)).bind(hash_secret(&cursor)).execute(&pool).await.expect("explicit expired fixture");
    assert_eq!(
        get(&app, &token, &format!("{}&cursor={expired}", list(&a, 2)))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(
        sqlx::query("UPDATE admin_audit_cursors SET before_sequence=1 WHERE cursor_hash=$1")
            .bind(hash_secret(&cursor))
            .execute(&pool)
            .await
            .is_err()
    );
    let (status, permissions) = get(
        &app,
        &token,
        &format!("/admin/v1/session/permissions?environment=dev&scope_kind=tenant&scope_id={a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(permissions["scope"]["scope_id"], a);
    assert!(
        permissions["permissions"]
            .as_array()
            .expect("permissions")
            .contains(&serde_json::json!("audit.read"))
    );
    // The same principal has a different role in a different scope. Its global
    // session union must not grant audit access or advertise it in this scope.
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'compute_operator','dev','tenant',$2,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(&principal).bind(&b).execute(&pool).await.expect("second scope with compute-only authority");
    let (status, permissions) = get(
        &app,
        &token,
        &format!("/admin/v1/session/permissions?environment=dev&scope_kind=tenant&scope_id={b}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !permissions["permissions"]
            .as_array()
            .expect("permissions")
            .contains(&serde_json::json!("audit.read"))
    );
    assert_eq!(
        get(&app, &token, &list(&b, 2)).await.0,
        StatusCode::FORBIDDEN
    );
    // Revocation blocks both saved cursor and direct immutable-event lookup.
    sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1")
        .bind(&principal)
        .execute(&pool)
        .await
        .expect("actual revoke");
    assert_eq!(get(&app, &token, &page2).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get(
            &app,
            &token,
            "/admin/v1/audit-events/legacy-a?environment=dev"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    pool.close().await;
}
