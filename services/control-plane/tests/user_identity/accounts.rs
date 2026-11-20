//! Actual user-session transactions and PG account facts; identity sessions are fixtures.
use super::*;
fn write_headers(f: &Fixture, key: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", key.parse().expect("key"));
    h
}
#[tokio::test]
async fn personal_budget_is_exact_owner_bound_and_does_not_create_credits() {
    let f = fixture().await;
    let other = fixture().await;
    let token = random_secret();
    let csrf = random_secret();
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(hash_secret(&token)).bind(Uuid::new_v4().to_string()).bind(&f.user).bind(&csrf).execute(&f.pool).await.expect("second session of same user");
    let body = json!({"scope_id":"precise-budget","limit":"9007199254740993"});
    let first = write_headers(&f, "same-budget");
    let mut second = first.clone();
    second.insert(
        "cookie",
        format!("{USER_COOKIE}={token}").parse().expect("cookie"),
    );
    second.insert("x-csrf-token", csrf.parse().expect("csrf"));
    let (a, b) = tokio::join!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(body.clone()),
            first
        ),
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(body.clone()),
            second
        )
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(a.1, b.1);
    assert_eq!(a.1["limit"], "9007199254740993");
    let id = a.1["root_budget_id"].as_str().expect("ID");
    let wallet = request(&f, "GET", "/api/v1/account/wallet", None, headers(&f)).await;
    assert_eq!(wallet.0, StatusCode::OK);
    assert_eq!(wallet.1["owner_id"], f.user);
    for key in [
        "gift",
        "earned",
        "purchased",
        "reserved_gift",
        "reserved_earned",
        "reserved_purchased",
        "pending_earned",
    ] {
        assert_eq!(wallet.1[key], "0");
    }
    assert_eq!(
        request(
            &other,
            "GET",
            &format!("/api/v1/account/budgets/{id}"),
            None,
            headers(&other)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(json!({"scope_id":"precise-budget","limit":"3"})),
            write_headers(&f, "same-budget")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(json!({"scope_id":"next-budget","limit":"0"})),
            write_headers(&f, "next-budget")
        )
        .await
        .0,
        StatusCode::OK
    );
    let page = request(
        &f,
        "GET",
        "/api/v1/account/budgets?limit=1",
        None,
        headers(&f),
    )
    .await;
    assert_eq!(page.1["data"].as_array().expect("rows").len(), 1);
    let cursor = page.1["next_cursor"].as_str().expect("bounded cursor");
    let next = request(
        &f,
        "GET",
        &format!("/api/v1/account/budgets?limit=1&after={cursor}"),
        None,
        headers(&f),
    )
    .await;
    assert_eq!(next.1["data"].as_array().expect("rows").len(), 1);
    assert!(next.1["next_cursor"].is_null());
    assert_ne!(
        page.1["data"][0]["root_budget_id"],
        next.1["data"][0]["root_budget_id"]
    );
}
#[tokio::test]
async fn personal_accounts_reject_owner_injection_csrf_invalid_money_and_revoked_replay() {
    let f = fixture().await;
    let body = json!({"scope_id":"scope","limit":"100"});
    let mut bad = headers(&f);
    bad.remove("x-csrf-token");
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(body.clone()),
            bad
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for value in [
        json!({"scope_id":"scope","limit":100}),
        json!({"scope_id":"scope","limit":"01"}),
        json!({"scope_id":"scope","limit":"100","owner_id":"victim"}),
    ] {
        assert_eq!(
            request(
                &f,
                "POST",
                "/api/v1/account/budgets",
                Some(value),
                headers(&f)
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        request(
            &f,
            "GET",
            "/api/v1/account/budgets?limit=101",
            None,
            headers(&f)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(body.clone()),
            headers(&f)
        )
        .await
        .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE user_sessions SET revoked=true WHERE user_id=$1")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("revoke");
    for path in ["/api/v1/account/wallet", "/api/v1/account/budgets"] {
        assert_eq!(
            request(&f, "GET", path, None, headers(&f)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/account/budgets",
            Some(body),
            headers(&f)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn budget_creation_rolls_back_if_session_expires_before_commit() {
    let f = fixture().await;
    let scope = Uuid::new_v4();
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION budget_expiry_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.scope_id='{scope}' THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE TRIGGER budget_expiry_{suffix} BEFORE INSERT ON budget_roots FOR EACH ROW EXECUTE FUNCTION budget_expiry_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(&f.pool)
        .await
        .expect("scoped delay");
    sqlx::query("UPDATE user_sessions SET idle_expires_at=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+2,absolute_expires_at=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+2 WHERE user_id=$1").bind(&f.user).execute(&f.pool).await.expect("short fixture session");
    let result = request(
        &f,
        "POST",
        "/api/v1/account/budgets",
        Some(json!({"scope_id":scope.to_string(),"limit":"99"})),
        write_headers(&f, "expiry-budget"),
    )
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER budget_expiry_{suffix} ON budget_roots; DROP FUNCTION budget_expiry_{suffix}();"))).execute(&f.pool).await.expect("remove exact fault");
    assert_eq!(result.0, StatusCode::UNAUTHORIZED);
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM budget_roots WHERE owner_id=$1),(SELECT count(*) FROM idempotency WHERE owner_id=$1 AND operation='user_create_budget.v1')").bind(&f.user).fetch_one(&f.pool).await.expect("rollback");
    assert_eq!(counts, (0, 0));
}

#[path = "support_consent.rs"]
mod support_consent;
