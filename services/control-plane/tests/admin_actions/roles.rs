//! Govern an existing operator's exact scoped role through real HTTP and PostgreSQL.
use super::*;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
const ACTIONS: &str = "/admin/v1/actions";
async fn setup() -> Fixture {
    let f = fixture().await;
    for (who, role) in [
        (&f.maker, "platform_admin"),
        (&f.checker, "security_operator"),
    ] {
        sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
            VALUES($1,$2,'dev','resource',$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)")
            .bind(&who.id).bind(role).bind(&f.node).execute(&f.pool).await.expect("scoped role governance fixture");
    }
    f
}
fn url(f: &Fixture, target: &str, role: &str) -> String {
    format!(
        "/admin/v1/principals/{target}/role-bindings/{role}?environment=dev&scope_kind=resource&scope_id={}",
        f.node
    )
}
async fn review(f: &Fixture, target: &str) -> Value {
    let (status, doc, _) = http(
        f,
        &f.maker,
        "GET",
        &url(f, target, "auditor"),
        "",
        "",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    doc
}
fn terms(view: &Value, expiry: Option<String>) -> Value {
    let kind = if expiry.is_some() {
        "admin.role_grant"
    } else {
        "admin.role_revoke"
    };
    let mut payload = json!({"principal_id":view["principal_id"],"role":"auditor"});
    if let Some(at) = expiry {
        payload["expires_at"] = json!(at);
    }
    json!({"action_type":kind,"ticket_id":"owned-role-ticket","reason":"Review the scoped operator role",
        "scope":view["scope"],"target_version":view["target_version"],"payload":payload})
}
fn expiry(after: i64) -> String {
    OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp() + after)
        .expect("future")
        .format(&Rfc3339)
        .expect("UTC")
}
async fn approve(f: &Fixture, body: Value, key: &str) -> (Value, String) {
    let (status, doc, tag) = http(f, &f.maker, "POST", ACTIONS, key, "", body).await;
    assert_eq!(status, StatusCode::CREATED, "{doc}");
    assert_eq!(doc["approval_policy"], "maker_checker");
    assert_eq!(
        http(
            f,
            &f.maker,
            "POST",
            &path(&doc, "approvals"),
            "self",
            &tag,
            decision(&doc, "approve")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, approved, ready) = http(
        f,
        &f.checker,
        "POST",
        &path(&doc, "approvals"),
        key,
        &tag,
        decision(&doc, "approve"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    (doc, ready)
}
#[tokio::test]
async fn bounded_role_grant_and_revoke_change_real_permissions_once() {
    let f = setup().await;
    let first = review(&f, &f.stranger.id).await;
    assert_eq!(first["state"], "absent");
    assert_eq!(first["revision"], "0");
    assert_eq!(
        http(
            &f,
            &f.stranger,
            "GET",
            &url(&f, &f.maker.id, "auditor"),
            "",
            "",
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let self_view = review(&f, &f.maker.id).await;
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            ACTIONS,
            "self-grant",
            "",
            terms(&self_view, Some(expiry(200)))
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
            ACTIONS,
            "excess",
            "",
            terms(&first, Some(expiry(4000)))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let body = terms(&first, Some(expiry(600)));
    let (action, tag) = approve(&f, body.clone(), "create-role").await;
    let permissions = format!(
        "/admin/v1/session/permissions?environment=dev&scope_kind=resource&scope_id={}",
        f.node
    );
    assert_eq!(
        http(&f, &f.stranger, "GET", &permissions, "", "", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let exec = path(&action, "execute");
    let result = http(&f, &f.maker, "POST", &exec, "issue", &tag, preview(&action)).await;
    assert_eq!(result.0, StatusCode::ACCEPTED, "{}", result.1);
    assert_eq!(
        result,
        http(&f, &f.maker, "POST", &exec, "issue", &tag, preview(&action)).await
    );
    let current = review(&f, &f.stranger.id).await;
    assert_eq!(current["state"], "active");
    assert_eq!(current["revision"], "1");
    assert_eq!(
        http(&f, &f.maker, "POST", ACTIONS, "old-view", "", body)
            .await
            .0,
        StatusCode::PRECONDITION_FAILED
    );
    let allowed = http(&f, &f.stranger, "GET", &permissions, "", "", Value::Null).await;
    assert_eq!(allowed.0, StatusCode::OK);
    assert!(
        allowed.1["permissions"]
            .as_array()
            .expect("permissions")
            .contains(&json!("audit.read"))
    );
    let self_checker = review(&f, &f.checker.id).await;
    let (status, self_action, self_tag) = http(
        &f,
        &f.maker,
        "POST",
        ACTIONS,
        "checker-target",
        "",
        terms(&self_checker, Some(expiry(300))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        http(
            &f,
            &f.checker,
            "POST",
            &path(&self_action, "approvals"),
            "target-review",
            &self_tag,
            decision(&self_action, "approve")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (cancel, tag) = approve(&f, terms(&current, None), "revoke-role").await;
    let final_state = http(
        &f,
        &f.maker,
        "POST",
        &path(&cancel, "execute"),
        "revoke-now",
        &tag,
        preview(&cancel),
    )
    .await;
    assert_eq!(final_state.0, StatusCode::ACCEPTED, "{}", final_state.1);
    let revoked = review(&f, &f.stranger.id).await;
    assert_eq!(revoked["revision"], "2");
    assert_eq!(revoked["state"], "revoked");
    assert_eq!(
        http(&f, &f.stranger, "GET", &permissions, "", "", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn restored_checker_role_does_not_validate_old_approval() {
    let f = setup().await;
    let view = review(&f, &f.stranger.id).await;
    let (doc, tag) = approve(&f, terms(&view, Some(expiry(300))), "needs-live-checker").await;
    for revoked in [true, false] {
        sqlx::query(
            "UPDATE admin_grants SET revoked=$2 WHERE principal_id=$1 AND role='security_operator'",
        )
        .bind(&f.checker.id)
        .bind(revoked)
        .execute(&f.pool)
        .await
        .expect("restore independent fixture role");
    }
    assert_eq!(
        http(
            &f,
            &f.maker,
            "POST",
            &path(&doc, "execute"),
            "run",
            &tag,
            preview(&doc)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(review(&f, &f.stranger.id).await["state"], "absent");
}
