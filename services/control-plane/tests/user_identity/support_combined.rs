//! Composed real user, support and security HTTP handlers; explicit login fixtures.
use super::*;
use orange_control_plane::admin::{
    self,
    identity::{AdminIdentity, OidcConfig},
};
use sqlx::Row;

struct Operator {
    id: String,
    token: String,
    csrf: String,
}
async fn operator(pool: &PgPool, issuer: &str, owner: &str, role: &str) -> Operator {
    let operator = Operator {
        id: Uuid::new_v4().to_string(),
        token: admin::random_secret(),
        csrf: admin::random_secret(),
    };
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&operator.id)
        .bind(issuer)
        .execute(pool)
        .await
        .expect("operator fixture");
    sqlx::query(
        "INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
        VALUES($1,$2,'dev','tenant',$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)",
    )
    .bind(&operator.id)
    .bind(role)
    .bind(owner)
    .execute(pool)
    .await
    .expect("tenant role");
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
        SELECT $1,$2,$3,t,t+600,t+3600 FROM (SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint t) n")
        .bind(admin::hash_secret(&operator.token)).bind(&operator.id).bind(&operator.csrf).execute(pool).await.expect("operator session");
    operator
}
struct Client<'a> {
    router: &'a Router,
    origin: &'a str,
    op: &'a Operator,
}
impl Client<'_> {
    async fn call(
        &self,
        method: &str,
        path: &str,
        key: &str,
        etag: &str,
        body: Value,
    ) -> (StatusCode, Value, String) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header(
                "cookie",
                format!("{}={}", admin::SESSION_COOKIE, self.op.token),
            )
            .header("origin", self.origin)
            .header("x-csrf-token", &self.op.csrf)
            .header("idempotency-key", key)
            .header("if-match", etag)
            .body(Body::from(body.to_string()))
            .expect("HTTP");
        let res = self.router.clone().oneshot(req).await.expect("handler");
        assert_eq!(res.headers()["cache-control"], "no-store");
        let tag = res
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 131072).await.expect("bounded");
        (status, serde_json::from_slice(&bytes).expect("JSON"), tag)
    }
}
pub(super) async fn verify(
    f: &Fixture,
    store: &ArtifactStore,
    file: &ArtifactView,
    terms: &Value,
    config: &str,
) {
    let cfg: OidcConfig =
        serde_json::from_slice(&std::fs::read(config).expect("actual configured admin discovery"))
            .expect("OIDC config");
    let maker = operator(&f.pool, &cfg.issuer, &f.user, "support_operator").await;
    let checker = operator(&f.pool, &cfg.issuer, &f.user, "security_operator").await;
    let other = operator(&f.pool, &cfg.issuer, &f.user, "support_operator").await;
    let origin = cfg.admin_origin.clone();
    let identity = AdminIdentity::discover(f.pool.clone(), cfg)
        .await
        .expect("real OIDC discovery")
        .with_support_reader(store.reader());
    let router = admin::routes::router(identity);
    let operator = |op| Client {
        op,
        origin: &origin,
        router: &router,
    };
    let support = operator(&maker);
    let security = operator(&checker);
    let stranger = operator(&other);
    let query = format!("environment=dev&scope_kind=tenant&scope_id={}", f.user);
    let grant_route = "/admin/v1/actions";
    if let Ok(driver) = std::env::var("ORANGE_SUPPORT_ADMIN_DRIVER") {
        let consent = request(
            f,
            "POST",
            BASE,
            Some(terms.clone()),
            write_headers(f, "admin-ui"),
        )
        .await;
        assert_eq!(consent.0, StatusCode::CREATED);
        browser(
            &router,
            &maker,
            &checker,
            &origin,
            file,
            consent.1["authorization_id"].as_str().expect("id"),
            driver,
        )
        .await;
    }
    for ending in [
        "operator",
        "user",
        "pending-restore",
        "withdrawn-checker",
        "deleted",
    ] {
        let authorized = request(
            f,
            "POST",
            BASE,
            Some(terms.clone()),
            write_headers(f, &format!("user-{ending}")),
        )
        .await;
        assert_eq!(authorized.0, StatusCode::CREATED, "{}", authorized.1);
        let auth = authorized.1["authorization_id"]
            .as_str()
            .expect("consent id");
        let (status, review, _) = support
            .call(
                "GET",
                &format!("/admin/v1/support-authorizations/{auth}?{query}"),
                "",
                "",
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{review}");
        let scope = json!({"environment":"dev","kind":"tenant","scope_id":f.user});
        let proposal = json!({"scope":scope,"target_version":review["target_version"],"ticket_id":terms["ticket_id"],"reason":"Review these user-selected bytes",
            "action_type":"support_access.grant","payload":{"subject_user_id":f.user,"user_authorization_id":auth,"resource_ids":[file.artifact_id],
            "content_fields":["file_content"],"expires_at":review["expires_at"]}});
        let mut expanded = proposal.clone();
        expanded["payload"]["resource_ids"] = json!([file.artifact_id, "unapproved"]);
        assert_eq!(
            support
                .call("POST", grant_route, "expanded", "", expanded)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, action, etag) = support.call("POST", grant_route, auth, "", proposal).await;
        assert_eq!(status, StatusCode::CREATED, "{action}");
        let base = format!(
            "{grant_route}/{}",
            action["action_id"].as_str().expect("action id")
        );
        let preview = json!({"policy_version":action["policy_version"],"payload_digest":action["payload_digest"]});
        let mut decision = preview.clone();
        decision["decision"] = json!("approve");
        decision["reason"] = json!("Verified the user authorization");
        assert_eq!(
            support
                .call(
                    "POST",
                    &format!("{base}/approvals"),
                    "self",
                    &etag,
                    decision.clone()
                )
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, approved, v2) = security
            .call("POST", &format!("{base}/approvals"), auth, &etag, decision)
            .await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        if ending == "pending-restore" {
            for revoked in [true, false] {
                sqlx::query("UPDATE admin_grants SET revoked=$2 WHERE principal_id=$1 AND role='security_operator'")
                    .bind(&checker.id).bind(revoked).execute(&f.pool).await.expect("withdraw old approval");
            }
            assert_eq!(
                support
                    .call(
                        "POST",
                        &format!("{base}/execute"),
                        "stale-approval",
                        &v2,
                        preview
                    )
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
            continue;
        }
        let (status, released, _) = support
            .call(
                "POST",
                &format!("{base}/execute"),
                &format!("execute-{auth}"),
                &v2,
                preview,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{released}");
        let grant = released["business_ref"].as_str().expect("support grant");
        let read = json!({"grant_id":grant,"resource_id":file.artifact_id,"content_field":"file_content","offset":0,"limit_bytes":5});
        let endpoint = "/admin/v1/support-content";
        assert_eq!(
            stranger
                .call("POST", endpoint, "foreign", "", read.clone())
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            security
                .call("POST", endpoint, "checker", "", read.clone())
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let first = support
            .call("POST", endpoint, grant, "", read.clone())
            .await;
        assert_eq!(first.0, StatusCode::OK, "{}", first.1);
        assert_eq!(first.1["content"], "owned");
        assert_eq!(first.1["truncated"], true);
        assert_eq!(
            first,
            support
                .call("POST", endpoint, grant, "", read.clone())
                .await
        );
        let mut altered = read.clone();
        altered["limit_bytes"] = json!(7);
        assert_eq!(
            support.call("POST", endpoint, grant, "", altered).await.0,
            StatusCode::CONFLICT
        );
        let audit=sqlx::query("SELECT artifact_id,version_id,returned_bytes FROM admin_support_reads WHERE audit_event_id=$1")
            .bind(first.1["audit_event_id"].as_str().expect("audit id")).fetch_one(&f.pool).await.expect("real audit receipt");
        assert_eq!(audit.get::<String, _>("version_id"), file.version_id);
        assert_eq!(audit.get::<i32, _>("returned_bytes"), 5);
        let detail = format!("/admin/v1/support-grants/{grant}?{query}");
        assert_eq!(
            support.call("GET", &detail, "", "", Value::Null).await.1["status"],
            "active"
        );
        match ending {
            "operator" => {
                let cancel = json!({"scope":scope,"target_version":"v1","ticket_id":terms["ticket_id"],"reason":"Stop further support access","action_type":"support_access.revoke","payload":{"grant_id":grant}});
                let (s, action, tag) = security
                    .call("POST", grant_route, "security-revoke", "", cancel)
                    .await;
                assert_eq!(s, StatusCode::CREATED, "{action}");
                assert_eq!(action["state"], "ready");
                let url = format!(
                    "{grant_route}/{}/execute",
                    action["action_id"].as_str().expect("id")
                );
                assert_eq!(security.call("POST",&url,"perform-revoke",&tag,json!({"policy_version":action["policy_version"],"payload_digest":action["payload_digest"]})).await.0,StatusCode::ACCEPTED);
            }
            "withdrawn-checker" => {
                for revoked in [true, false] {
                    sqlx::query("UPDATE admin_grants SET revoked=$2 WHERE principal_id=$1 AND role='security_operator'")
                        .bind(&checker.id).bind(revoked).execute(&f.pool).await.expect("withdraw and restore only the fixture role");
                }
            }
            "user" => {
                assert_eq!(
                    request(f, "DELETE", &format!("{BASE}/{auth}"), None, headers(f))
                        .await
                        .0,
                    StatusCode::OK
                );
            }
            _ => {
                let actor = f
                    .identity
                    .artifact_actor(&headers(f), true)
                    .await
                    .expect("user");
                store
                    .delete(&actor, &f.user, &file.upload_id)
                    .await
                    .expect("actual file tombstone");
            }
        }
        let denied = support.call("POST", endpoint, grant, "", read).await;
        assert!(denied.0.is_client_error());
        assert!(denied.1.get("content").is_none());
        assert_eq!(
            support.call("GET", &detail, "", "", Value::Null).await.1["status"],
            if ending == "deleted" {
                "unavailable"
            } else {
                "revoked"
            }
        );
    }
}

async fn browser(
    router: &Router,
    maker: &Operator,
    checker: &Operator,
    origin: &str,
    file: &ArtifactView,
    consent: &str,
    driver: String,
) {
    use std::{path::PathBuf, process::Stdio};
    use tokio::io::AsyncWriteExt;
    let folder =
        PathBuf::from(std::env::var("ORANGE_SUPPORT_ADMIN_EVIDENCE").expect("owned evidence"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("private Admin listener");
    let addr = listener.local_addr().expect("actual port");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let app = router.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let mut child = tokio::process::Command::new(
        std::env::var("ORANGE_SUPPORT_ADMIN_NODE").expect("locked Node"),
    )
    .arg(driver)
    .env_clear()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .expect("browser test");
    let body = json!({"api":format!("http://{addr}"),"origin":origin,"maker":maker.token,"checker":checker.token,"artifact":file.artifact_id,
        "version":file.version_id,"authorization":consent,"screenshot":folder.join("revoked.png")});
    let mut input = child.stdin.take().expect("private stdin");
    input
        .write_all(&serde_json::to_vec(&body).expect("fixture"))
        .await
        .expect("fixture input");
    drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("bounded browser")
        .expect("driver exit");
    stop.send(()).expect("stop own router");
    server.await.expect("server task").expect("clean stop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).expect("browser receipt");
    assert_eq!(receipt["mutations"], 6);
    assert_eq!(receipt["revoked"], true);
    std::fs::write(
        folder.join("browser-result.json"),
        serde_json::to_vec_pretty(&receipt).expect("report"),
    )
    .expect("save report");
}
