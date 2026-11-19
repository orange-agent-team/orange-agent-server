use super::*;

const LIST: &str = "/admin/v1/reconciliation-cases";
fn query(scope: &str, kind: &str, limit: u16) -> String {
    format!("{LIST}?environment=dev&scope_kind={kind}&scope_id={scope}&limit={limit}")
}
async fn seed_case(pool: &PgPool, id: &str, owner: &str, env: &str) {
    sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at) VALUES($1,$2,$3,$1,'waiting_external',1700000000)").bind(id).bind(owner).bind(env).execute(pool).await.expect("case fixture");
}
pub(super) async fn exercise(pool: &PgPool, app: &Router, issuer: &str) {
    let a = "case-tenant-a";
    let b = "case-tenant-b";
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1),($2)")
        .bind(a)
        .bind(b)
        .execute(pool)
        .await
        .expect("owned fixture tenants");
    let (principal, token) = actor(pool, issuer, "tenant", a).await;
    let (_, other) = actor(pool, issuer, "tenant", b).await;
    let (_, platform) = actor(pool, issuer, "platform", "platform").await;
    for (id, owner, env) in [
        ("case-1", a, "dev"),
        ("case-2", a, "dev"),
        ("case-b", b, "dev"),
        ("case-staging", a, "staging"),
    ] {
        seed_case(pool, id, owner, env).await;
    }
    let list = query(a, "tenant", 1);
    let (status, first) = get(app, &token, &list).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let expected = serde_json::json!({"case_id":"case-2","business_ref":"case-2","owner_id":a,"state":"waiting_external","next_check_at":"2023-11-14T22:13:20Z","version":"1"});
    assert_eq!(first["data"], serde_json::json!([expected.clone()]));
    let cursor = first["next_cursor"].as_str().expect("cursor");
    seed_case(pool, "case-later", a, "dev").await;
    let page = format!("{list}&cursor={cursor}");
    let second = get(app, &token, &page).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["data"][0]["case_id"], "case-1");
    assert!(second.1.get("next_cursor").is_none());
    let other_page = format!("{}&cursor={cursor}", query(b, "tenant", 1));
    assert_eq!(
        get(app, &other, &other_page).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        get(app, &session(pool, &principal).await, &page).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(get(app,&token,&format!("/admin/v1/audit-events?environment=dev&scope_kind=tenant&scope_id={a}&cursor={cursor}")).await.0,StatusCode::UNPROCESSABLE_ENTITY);
    let detail = format!("{LIST}/case-2?environment=dev");
    assert_eq!(get(app, &token, &detail).await.1, expected);
    for path in [detail.as_str(), &format!("{LIST}/absent?environment=dev")] {
        assert_eq!(get(app, &other, path).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        get(app, &platform, &query("platform", "platform", 100))
            .await
            .1["data"]
            .as_array()
            .expect("all dev")
            .len(),
        4
    );
    assert_eq!(
        get(app, &token, &format!("{LIST}/case-staging?environment=dev"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(app, &token, &query(a, "tenant", 201)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        get(app, &token, &query(b, "tenant", 5)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(app, &token, &query(a, "region", 1)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM admin_audit WHERE principal_id=$1 AND event_type='reconciliation_case.read' AND scope_id=$2 AND entity_id='case-2' AND request_id='audit-test-request'").bind(&principal).bind(a).fetch_one(pool).await.expect("scoped audit"),1);
    sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1")
        .bind(&principal)
        .execute(pool)
        .await
        .expect("withdraw");
    assert_eq!(get(app, &token, &page).await.0, StatusCode::FORBIDDEN);
    // With all grants withdrawn the session is denied before either lookup.
    for path in [
        detail.as_str(),
        "/admin/v1/reconciliation-cases/absent?environment=dev",
    ] {
        assert_eq!(get(app, &token, path).await.0, StatusCode::FORBIDDEN);
    }
}
