use super::*;

const KEYS: &str = "/api/v1/account/api-keys";
const WALLET: &str = "/api/v1/automation/account/wallet";
const BUDGETS: &str = "/api/v1/automation/account/budgets";
fn credential() -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let token = format!("oa1.{id}.{}", hash_secret(&random_secret()));
    (id, token)
}
fn registration(token: &str, scope: &str) -> Value {
    json!({"scope":scope,"audience":"orange-personal-automation-v1","label":"monthly report","ttl_seconds":3600,"verifier":hash_secret(token)})
}
fn management(f: &Fixture, id: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", id.parse().expect("ID"));
    h
}
fn bearer(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {token}").parse().expect("token header"),
    );
    h
}

async fn second_management_session(f: &Fixture, id: &str) -> HeaderMap {
    let second_token = random_secret();
    let second_csrf = random_secret();
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
        SELECT $1,$2,user_id,$3,auth_time,idle_expires_at,absolute_expires_at FROM user_sessions WHERE token_hash=$4")
        .bind(hash_secret(&second_token)).bind(Uuid::new_v4().to_string()).bind(&second_csrf).bind(hash_secret(&f.token))
        .execute(&f.pool).await.expect("independent concurrent session");
    let mut second = management(f, id);
    second.insert(
        "cookie",
        format!("{USER_COOKIE}={second_token}")
            .parse()
            .expect("test cookie"),
    );
    second.insert("x-csrf-token", second_csrf.parse().expect("test CSRF"));
    second
}

#[tokio::test]
async fn personal_key_lifecycle_binds_owner_scope_and_original_intent() {
    let f = fixture().await;
    let other = fixture().await;
    let (id, token) = credential();
    let path = format!("{KEYS}/{id}");
    let body = registration(&token, "wallet:read");
    let headers = management(&f, &id);
    let mut no_csrf = headers.clone();
    no_csrf.remove("x-csrf-token");
    assert_eq!(
        request(&f, "PUT", &path, Some(body.clone()), no_csrf)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut wrong = body.clone();
    wrong["audience"] = json!("orange-admin");
    assert_eq!(
        request(&f, "PUT", &path, Some(wrong), headers.clone())
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let second = second_management_session(&f, &id).await;
    let (a, b) = tokio::join!(
        request(&f, "PUT", &path, Some(body.clone()), headers.clone()),
        request(&f, "PUT", &path, Some(body.clone()), second)
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(a, b);
    assert_eq!(a.1["user_id"], f.user);
    assert_eq!(a.1["status"], "active");
    assert!(a.1.get("verifier").is_none() && !a.1.to_string().contains(&token));
    let persisted:String=sqlx::query_scalar("SELECT to_jsonb(k)::text FROM personal_automation_keys k WHERE user_id=$1 AND key_id=$2::text::uuid")
        .bind(&f.user).bind(&id).fetch_one(&f.pool).await.expect("stored metadata");
    assert!(persisted.contains(&hash_secret(&token)) && !persisted.contains(&token));
    for (field, value) in [
        ("scope", json!("budgets:read")),
        ("label", json!("changed")),
        ("ttl_seconds", json!(7200)),
        ("verifier", json!("f".repeat(64))),
    ] {
        let mut altered = body.clone();
        altered[field] = value;
        assert_eq!(
            request(&f, "PUT", &path, Some(altered), headers.clone())
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    sqlx::query("UPDATE wallets SET gift=9007199254740993 WHERE owner_id=$1")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("fixture balance");
    let (status, w) = request(&f, "GET", WALLET, None, bearer(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w["owner_id"], f.user);
    assert_eq!(w["gift"], "9007199254740993");
    assert_eq!(
        request(&f, "GET", BUDGETS, None, bearer(&token)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&f, "GET", KEYS, None, bearer(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(json!({"scope_id":"no","limit":"1"})),
            bearer(&token)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mut mixed = bearer(&token);
    mixed.insert("cookie", headers["cookie"].clone());
    assert_eq!(
        request(&f, "GET", WALLET, None, mixed).await.0,
        StatusCode::UNAUTHORIZED
    );
    let list = request(&f, "GET", KEYS, None, headers.clone()).await.1;
    assert_eq!(list["data"].as_array().expect("page").len(), 1);
    assert!(list["data"][0].get("verifier").is_none());
    assert_eq!(
        request(&other, "GET", KEYS, None, management(&other, &id))
            .await
            .1["data"],
        json!([])
    );
    assert_eq!(
        request(&other, "DELETE", &path, None, management(&other, &id))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&token)).await.0,
        StatusCode::OK
    );
    for _ in 0..2 {
        assert_eq!(
            request(&f, "DELETE", &path, None, headers.clone()).await.0,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        request(&f, "PUT", &path, Some(body), headers.clone())
            .await
            .1["status"],
        "revoked"
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT e.event_type FROM durable_events e JOIN event_streams s USING(stream_id)
        WHERE s.entity_kind='api_key' AND s.entity_id=$1 AND s.owner_id=$2 ORDER BY e.seq",
    )
    .bind(&id)
    .bind(&f.user)
    .fetch_all(&f.pool)
    .await
    .expect("committed lifecycle events");
    assert_eq!(audit, ["api_key.registered", "api_key.withdrawn"]);
    let (id, token) = credential();
    let path = format!("{KEYS}/{id}");
    assert_eq!(
        request(&f, "DELETE", &path, None, management(&f, &id))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &f,
            "PUT",
            &path,
            Some(registration(&token, "wallet:read")),
            management(&f, &id)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn budget_key_never_outlives_expiry_or_withdrawn_account_authority() {
    let f = fixture().await;
    let other = fixture().await;
    for actor in [&f, &other] {
        let resp = request(
            actor,
            "POST",
            "/api/v1/account/budgets",
            Some(json!({"scope_id":"work","limit":"13"})),
            headers(actor),
        )
        .await;
        assert_eq!(resp.0, StatusCode::OK);
    }
    let (id, token) = credential();
    let path = format!("{KEYS}/{id}");
    assert_eq!(
        request(
            &f,
            "PUT",
            &path,
            Some(registration(&token, "budgets:read")),
            management(&f, &id)
        )
        .await
        .0,
        StatusCode::OK
    );
    let (s, page) = request(&f, "GET", BUDGETS, None, bearer(&token)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["user_id"], f.user);
    assert_eq!(page["data"].as_array().expect("rows").len(), 1);
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&token)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f,
            "GET",
            &format!("{BUDGETS}?owner_id={}", other.user),
            None,
            bearer(&token)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for state in ["restricted", "active"] {
        sqlx::query("UPDATE owners SET state=$1 WHERE owner_id=$2")
            .bind(state)
            .bind(&f.user)
            .execute(&f.pool)
            .await
            .expect("state");
    }
    assert_eq!(
        request(&f, "GET", BUDGETS, None, bearer(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (id, token) = credential();
    sqlx::query("WITH t AS (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint-61 AS n)
        INSERT INTO personal_automation_keys(user_id,key_id,owner_id,scope,verifier,label,created_unix,ttl_seconds,expires_unix)
        SELECT $1,$2::text::uuid,$1,'budgets:read',$3,'old',n,60,n+60 FROM t")
        .bind(&f.user).bind(&id).bind(hash_secret(&token)).execute(&f.pool).await.expect("expired record");
    assert_eq!(
        request(&f, "GET", BUDGETS, None, bearer(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&f, "GET", BUDGETS, None, headers(&f)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn logout_keeps_delegation_and_user_disablement_permanently_withdraws_it() {
    let f = fixture().await;
    let (id, token) = credential();
    let path = format!("{KEYS}/{id}");
    assert_eq!(
        request(
            &f,
            "PUT",
            &path,
            Some(registration(&token, "wallet:read")),
            management(&f, &id)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&f, "DELETE", "/api/v1/session", None, headers(&f))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&f, "GET", KEYS, None, headers(&f)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&token)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &f,
            "GET",
            "/api/v1/automation/account/wallet?owner_id=other",
            None,
            bearer(&token)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for active in [false, true] {
        sqlx::query("UPDATE users SET active=$1 WHERE user_id=$2")
            .bind(active)
            .bind(&f.user)
            .execute(&f.pool)
            .await
            .expect("account status");
        assert_eq!(
            request(&f, "GET", WALLET, None, bearer(&token)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn key_read_rechecks_database_time_after_an_authority_lock_wait() {
    use std::time::Duration;
    let f = fixture().await;
    let (id, token) = credential();
    let mut block = f.pool.begin().await.expect("blocker");
    sqlx::query("WITH t AS (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint-55 AS n)
        INSERT INTO personal_automation_keys(user_id,key_id,owner_id,scope,verifier,label,created_unix,ttl_seconds,expires_unix)
        SELECT $1,$2::text::uuid,$1,'wallet:read',$3,'brief',n,60,n+60 FROM t")
        .bind(&f.user).bind(&id).bind(hash_secret(&token)).execute(&f.pool).await.expect("short lease");
    let pid:i32=sqlx::query_scalar("SELECT pg_backend_pid() FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid FOR UPDATE")
        .bind(&f.user).bind(&id).fetch_one(&mut *block).await.expect("lock");
    let reader = request(&f, "GET", WALLET, None, bearer(&token));
    let release = async {
        tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                    .bind(pid).fetch_one(&f.pool).await.expect("blocked read");
                if waiting{break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("read blocked on actual key");
        sqlx::query("SELECT pg_sleep_until(to_timestamp(expires_unix)+interval '10 milliseconds') FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid")
            .bind(&f.user).bind(id).execute(&mut *block).await.expect("expire before release");
        block.commit().await.expect("unlock");
    };
    let ((status, body), ()) = tokio::join!(reader, release);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.get("gift").is_none());
}

#[cfg(feature = "personal-key-browser-tests")]
#[tokio::test]
async fn real_personal_key_browser_management() {
    use tokio::io::AsyncWriteExt;
    let f = fixture().await;
    let driver = std::env::var("ORANGE_TEST_KEY_BROWSER").expect("real browser driver");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("private API");
    let addr = listener.local_addr().expect("bound");
    let (stop, done) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(async move {
        axum::serve(listener, f.router)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .expect("serve")
    });
    let mut child =
        tokio::process::Command::new(std::env::var("ORANGE_TEST_KEY_NODE").expect("node"))
            .arg(driver)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("browser");
    let mut input = child.stdin.take().expect("fixture pipe");
    input.write_all(&serde_json::to_vec(&json!({"api":format!("http://{addr}"),"origin":f.identity.config.user_origin,"user":f.user,"token":f.token})).expect("session fixture")).await.expect("deliver");
    drop(input);
    let status = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("browser deadline")
        .expect("browser status");
    let _ = stop.send(());
    service.await.expect("server stopped");
    assert!(status.status.success(), "browser process");
    let report: Value = serde_json::from_slice(&status.stdout).expect("bounded fixture result");
    for mark in [
        "real_browser",
        "lost_response_same_key",
        "budget_read",
        "wallet_scope_denied",
        "no_secret_in_web_storage",
        "reopening_hides_secret",
        "revoke_denied_future_reads",
        "withdraw_before_create",
        "original_revoked_at_replacement",
        "recovered_successor",
        "successor_withdrawn",
    ] {
        assert_eq!(report[mark], true, "missing browser observation {mark}");
    }
}

#[tokio::test]
async fn replacement_retires_once_and_an_uncommitted_successor_cannot_retire_anything() {
    let f = fixture().await;
    let other = fixture().await;
    let (old, original) = credential();
    let old_path = format!("{KEYS}/{old}");
    let original_body = registration(&original, "wallet:read");
    assert_eq!(
        request(
            &f,
            "PUT",
            &old_path,
            Some(original_body.clone()),
            management(&f, &old)
        )
        .await
        .0,
        StatusCode::OK
    );
    let (new, secret) = credential();
    let replace = |id: &str| format!("{KEYS}/{old}/replacements/{id}");
    assert_eq!(
        request(
            &other,
            "PUT",
            &replace(&new),
            Some(registration(&secret, "wallet:read")),
            management(&other, &new)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &f,
            "PUT",
            &replace(&new),
            Some(registration(&secret, "budgets:read")),
            management(&f, &new)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&original)).await.0,
        StatusCode::OK
    );
    let (abandoned, abandoned_secret) = credential();
    assert_eq!(
        request(
            &f,
            "DELETE",
            &format!("{KEYS}/{abandoned}"),
            None,
            management(&f, &abandoned)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &f,
            "PUT",
            &replace(&abandoned),
            Some(registration(&abandoned_secret, "wallet:read")),
            management(&f, &abandoned)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    // Force the successor's own final outbox write to fail after the predecessor would be retired.
    let (fault, uncommitted) = credential();
    sqlx::query("INSERT INTO event_streams(owner_id,entity_kind,entity_id,next_seq) VALUES($1,'api_key',$2,9223372036854775807)")
        .bind(&f.user).bind(&fault).execute(&f.pool).await.expect("isolated exhausted fixture stream");
    assert_eq!(
        request(
            &f,
            "PUT",
            &replace(&fault),
            Some(registration(&uncommitted, "wallet:read")),
            management(&f, &fault)
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&original)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&uncommitted))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid",
    )
    .bind(&f.user)
    .bind(&fault)
    .fetch_one(&f.pool)
    .await
    .expect("no failed successor");
    assert_eq!(count, 0);

    let (other_id, second_secret) = credential();
    let other_session = second_management_session(&f, &other_id).await;
    let path_a = replace(&new);
    let path_b = replace(&other_id);
    let (a, b) = tokio::join!(
        request(
            &f,
            "PUT",
            &path_a,
            Some(registration(&secret, "wallet:read")),
            management(&f, &new)
        ),
        request(
            &f,
            "PUT",
            &path_b,
            Some(registration(&second_secret, "wallet:read")),
            other_session
        )
    );
    let (winner, winning_secret, losing_secret, first) = match (a.0, b.0) {
        (StatusCode::OK, StatusCode::CONFLICT) => (new, secret, second_secret, a.1),
        (StatusCode::CONFLICT, StatusCode::OK) => (other_id, second_secret, secret, b.1),
        _ => panic!("one rotation must win"),
    };
    let body = registration(&winning_secret, "wallet:read");
    let retry = request(
        &f,
        "PUT",
        &replace(&winner),
        Some(body.clone()),
        management(&f, &winner),
    )
    .await;
    assert_eq!(retry, (StatusCode::OK, first));
    assert_eq!(
        request(
            &f,
            "PUT",
            &format!("{KEYS}/{winner}"),
            Some(body),
            management(&f, &winner)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &f,
            "PUT",
            &old_path,
            Some(original_body),
            management(&f, &old)
        )
        .await
        .1["status"],
        "revoked"
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&original)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&losing_secret))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&winning_secret))
            .await
            .0,
        StatusCode::OK
    );
    let totals:(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE entity_id=$2 AND event_type='api_key.withdrawn'),
        count(*) FILTER(WHERE entity_id=$3 AND event_type='api_key.registered') FROM outbox WHERE owner_id=$1")
        .bind(&f.user).bind(&old).bind(&winner).fetch_one(&f.pool).await.expect("pair of notices");
    assert_eq!(totals, (1, 1));
}

#[tokio::test]
async fn a_rotation_queued_on_an_expiring_predecessor_creates_no_new_authority() {
    use std::time::Duration;
    let f = fixture().await;
    let (old, token) = credential();
    let (id, next) = credential();
    sqlx::query("WITH t AS (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint-55 AS n)
        INSERT INTO personal_automation_keys(user_id,key_id,owner_id,scope,verifier,label,created_unix,ttl_seconds,expires_unix)
        SELECT $1,$2::text::uuid,$1,'wallet:read',$3,'short',n,60,n+60 FROM t")
        .bind(&f.user).bind(&old).bind(hash_secret(&token)).execute(&f.pool).await.expect("expiring original");
    let mut block = f.pool.begin().await.expect("blocker");
    let pid:i32=sqlx::query_scalar("SELECT pg_backend_pid() FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid FOR UPDATE")
        .bind(&f.user).bind(&old).fetch_one(&mut *block).await.expect("lock original");
    let path = format!("{KEYS}/{old}/replacements/{id}");
    let rotate = request(
        &f,
        "PUT",
        &path,
        Some(registration(&next, "wallet:read")),
        management(&f, &id),
    );
    let release = async {
        tokio::time::timeout(Duration::from_secs(3),async {
            loop{
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                    .bind(pid).fetch_one(&f.pool).await.expect("competing admission");
                if waiting{break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("wait before expiry");
        sqlx::query("SELECT pg_sleep_until(to_timestamp(expires_unix)+interval '20 milliseconds') FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid")
            .bind(&f.user).bind(&old).execute(&mut *block).await.expect("expire original");
        block.commit().await.expect("release");
    };
    let ((status, _), ()) = tokio::join!(rotate, release);
    assert_eq!(status, StatusCode::CONFLICT);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid",
    )
    .bind(&f.user)
    .bind(&id)
    .fetch_one(&f.pool)
    .await
    .expect("no successor");
    assert_eq!(count, 0);
    assert_eq!(
        request(&f, "GET", WALLET, None, bearer(&next)).await.0,
        StatusCode::UNAUTHORIZED
    );
}
