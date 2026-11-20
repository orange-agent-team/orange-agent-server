//! Real session, PG consent records and OpenDAL-backed artifact versions.
use super::*;
use orange_control_plane::artifacts::ArtifactStore;
use orange_protocol::artifacts::{ArtifactView, CreateUpload};
use sha2::{Digest, Sha256};
const BASE: &str = "/api/v1/account/support-authorizations";

async fn upload(f: &Fixture, store: &ArtifactStore, media: &str) -> ArtifactView {
    let actor = f
        .identity
        .artifact_actor(&headers(f), true)
        .await
        .expect("user identity");
    let bytes = b"owned support diagnostic".to_vec();
    let upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: media.to_owned(),
    };
    store
        .begin_upload(&actor, &f.user, &upload)
        .await
        .expect("create upload");
    store
        .put(
            actor,
            f.user.clone(),
            upload.upload_id,
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("publish bytes")
}
fn resource(a: &ArtifactView) -> Value {
    json!({"artifact_id":a.artifact_id,"version_id":a.version_id})
}
fn terms(a: &ArtifactView) -> Value {
    json!({"ticket_id":"ticket-123","purpose":"Check this diagnostic file","resources":[resource(a)],"content_field":"file_content","duration_seconds":300})
}
#[tokio::test]
async fn exact_personal_text_version_requires_explicit_consent_and_permanent_revocation() {
    let f = fixture().await;
    let other = fixture().await;
    let dir = tempfile::tempdir().expect("object root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private root");
    }
    let store = ArtifactStore::open_local(f.pool.clone(), dir.path(), 1024 * 1024, 100)
        .await
        .expect("owned object store");
    let first = upload(&f, &store, "text/plain").await;
    let content = terms(&first);
    let visible = request(
        &f,
        "GET",
        "/api/v1/account/support-resources",
        None,
        headers(&f),
    )
    .await;
    assert_eq!(visible.0, StatusCode::OK);
    assert_eq!(visible.1["data"][0]["version_id"], first.version_id);
    assert_eq!(
        request(
            &other,
            "GET",
            "/api/v1/account/support-resources",
            None,
            headers(&other)
        )
        .await
        .1["data"],
        json!([])
    );
    let mut csrf = headers(&f);
    csrf.remove("x-csrf-token");
    assert_eq!(
        request(&f, "POST", BASE, Some(content.clone()), csrf)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&other, "POST", BASE, Some(content.clone()), headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let binary = upload(&f, &store, "application/octet-stream").await;
    assert_eq!(
        request(&f, "POST", BASE, Some(terms(&binary)), headers(&f))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for (key, value) in [
        ("content_field", json!("memory")),
        ("duration_seconds", json!(86401)),
        ("resources", json!([resource(&first), resource(&first)])),
        ("owner_id", json!(other.user)),
    ] {
        let mut invalid = content.clone();
        invalid[key] = value;
        assert_eq!(
            request(&f, "POST", BASE, Some(invalid), headers(&f))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut wrong = content.clone();
    wrong["resources"][0]["version_id"] = json!(binary.version_id);
    assert_eq!(
        request(&f, "POST", BASE, Some(wrong), headers(&f)).await.0,
        StatusCode::NOT_FOUND
    );
    let (a, b) = tokio::join!(
        request(&f, "POST", BASE, Some(content.clone()), headers(&f)),
        request(&f, "POST", BASE, Some(content.clone()), headers(&f))
    );
    assert_eq!(a.0, StatusCode::CREATED, "{}", a.1);
    assert_eq!(a, b);
    let id = a.1["authorization_id"].as_str().expect("id");
    let endpoint = format!("{BASE}/{id}");
    assert_eq!(a.1["status"], "active");
    assert!(!a.1.to_string().contains("owned support diagnostic"));
    assert_eq!(
        request(&other, "GET", &endpoint, None, headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&other, "DELETE", &endpoint, None, headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let mut changed = content.clone();
    changed["purpose"] = json!("Different purpose");
    assert_eq!(
        request(&f, "POST", BASE, Some(changed), headers(&f))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let deleted = upload(&f, &store, "text/plain").await;
    let second = request(
        &f,
        "POST",
        BASE,
        Some(terms(&deleted)),
        write_headers(&f, "delete-later"),
    )
    .await;
    assert_eq!(second.0, StatusCode::CREATED);
    let page = request(&f, "GET", &format!("{BASE}?limit=1"), None, headers(&f)).await;
    assert_eq!(page.1["data"][0], second.1);
    let next = format!(
        "{BASE}?limit=1&before={}",
        page.1["next_before"].as_str().expect("next page")
    );
    assert_eq!(
        request(&other, "GET", &next, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&f, "GET", &next, None, headers(&f)).await.1["data"][0],
        a.1
    );
    let actor = f
        .identity
        .artifact_actor(&headers(&f), true)
        .await
        .expect("user actor");
    store
        .delete(&actor, &f.user, &deleted.upload_id)
        .await
        .expect("tombstone");
    let unavailable = format!(
        "{BASE}/{}",
        second.1["authorization_id"]
            .as_str()
            .expect("authorization")
    );
    assert_eq!(
        request(&f, "GET", &unavailable, None, headers(&f)).await.1["status"],
        "unavailable"
    );
    let revoked = request(&f, "DELETE", &endpoint, None, headers(&f)).await;
    assert_eq!(revoked.0, StatusCode::OK);
    assert_eq!(revoked.1["status"], "revoked");
    assert_eq!(
        request(&f, "DELETE", &endpoint, None, headers(&f)).await,
        revoked
    );
    assert_eq!(
        request(&f, "POST", BASE, Some(content.clone()), headers(&f))
            .await
            .1,
        revoked.1
    );
    let mut brief = content;
    brief["duration_seconds"] = json!(1);
    let timed = request(&f, "POST", BASE, Some(brief), write_headers(&f, "short")).await;
    let id = timed.1["authorization_id"]
        .as_str()
        .expect("short authorization");
    sqlx::query("SELECT pg_sleep(GREATEST(0,expires_at - extract(epoch FROM clock_timestamp()))) FROM personal_support_consents WHERE authorization_id=$1")
        .bind(id).execute(&f.pool).await.expect("database deadline");
    assert_eq!(
        request(&f, "GET", &format!("{BASE}/{id}"), None, headers(&f))
            .await
            .1["status"],
        "expired"
    );
    if let Ok(driver) = std::env::var("ORANGE_SUPPORT_BROWSER_DRIVER") {
        browser_consent_and_revocation_with_real_user_router(driver).await;
    }
}

async fn browser_consent_and_revocation_with_real_user_router(driver: String) {
    use std::{path::PathBuf, process::Stdio};
    use tokio::io::AsyncWriteExt;
    let f = fixture().await;
    let dir = tempfile::tempdir().expect("private root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private root");
    }
    let store = ArtifactStore::open_local(f.pool.clone(), dir.path(), 1024 * 1024, 100)
        .await
        .expect("owned object store");
    let text = upload(&f, &store, "text/plain").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned user API listener");
    let addr = listener.local_addr().expect("address");
    let app = f.router.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let folder = PathBuf::from(std::env::var("ORANGE_SUPPORT_EVIDENCE").expect("evidence"));
    let mut child = tokio::process::Command::new(
        std::env::var("ORANGE_SUPPORT_BROWSER_NODE").expect("pinned node"),
    )
    .arg(driver)
    .env_clear()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .expect("browser driver");
    let bytes=serde_json::to_vec(&json!({"api":format!("http://{addr}"),"origin":"https://127.0.0.1:43122","user":f.user,"token":f.token,
        "artifact":text.artifact_id,"version":text.version_id,"screenshot":folder.join("support-revoked.png")})).expect("private fixture");
    let mut input = child.stdin.take().expect("driver input");
    input.write_all(&bytes).await.expect("supply fixture");
    drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("bounded browser")
        .expect("exit");
    stop.send(()).expect("stop listener");
    server
        .await
        .expect("server task")
        .expect("graceful shutdown");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("typed browser result");
    assert_eq!(result["mutations"], 2);
    assert_eq!(result["revoked_after_reload"], true);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM personal_support_consents c JOIN personal_support_revocations r USING(authorization_id)
        WHERE c.user_id=$1 AND c.authorization_id=$2 AND c.resource_versions=$3")
        .bind(&f.user).bind(result["authorization_id"].as_str().expect("result id")).bind(json!([resource(&text)]))
        .fetch_one(&f.pool).await.expect("independent revoked version receipt");
    assert_eq!(count, 1);
    std::fs::write(
        folder.join("browser-result.json"),
        serde_json::to_vec_pretty(&result).expect("report"),
    )
    .expect("browser evidence");
}
