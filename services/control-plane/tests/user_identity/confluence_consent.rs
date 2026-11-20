use super::connector_run::{active_run, keyed, opted_in};
use super::*;
use orange_control_plane::{
    artifacts::ArtifactStore,
    confluence::{Credential, fixtures, registry::Registered},
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
#[tokio::test]
async fn confluence_page_scope_quarantine_and_release_after_revoke() {
    let mut f = fixture().await;
    let other = fixture().await;
    let (legacy, _original_root) = active_run(&f).await;
    let backend = fixtures::fixture().await;
    let registration = backend.registration();
    let reader = registration
        .open_loopback_fixture(
            backend.site,
            backend.media_addr,
            &Credential::CloudOauth {
                token: "fixture-confluence-token".into(),
            },
        )
        .expect("registered fixture");
    f.identity.confluence_sources.0.insert(
        "wiki".into(),
        Arc::new(Registered {
            owner: f.user.clone(),
            digest: "e".repeat(64),
            registration,
            reader,
        }),
    );
    let private = tempfile::tempdir().expect("isolated artifact writer");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private root");
    let store = ArtifactStore::open_local(f.pool.clone(), private.path(), 5 * 1024 * 1024, 10)
        .await
        .expect("actual OpenDAL writer");
    f.router = auth::routes::router(f.identity.clone()).merge(auth::routes::confluence_router(
        f.identity.clone(),
        Some(store.clone()),
    ));
    let terms = json!({"source_id":"wiki","source_digest":"e".repeat(64),"space_keys":["ops"],"duration_seconds":180});
    let old = format!("/api/v1/agent/runs/{legacy}/connectors/confluence");
    assert_eq!(
        request(
            &f,
            "POST",
            &old,
            Some(terms.clone()),
            keyed(&f, "no-opt-in")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let run = opted_in(&f, &legacy, "allow_document_connectors").await;
    let base = format!("/api/v1/agent/runs/{run}/connectors/confluence");
    assert_eq!(
        request(&f, "GET", &base, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let (code, g) = request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve")).await;
    assert_eq!(code, StatusCode::OK, "{g}");
    let id = g["grant_id"].as_str().expect("grant");
    let granted = format!("{base}/{id}");
    let actions = format!("{granted}/actions");
    let query = json!({"action":"search_pages","space":"ops","query":"release","limit":5});
    let before = backend.hits.load(Ordering::SeqCst);
    assert_eq!(
        request(
            &f,
            "POST",
            &actions,
            Some(json!({"action":"search_pages","space":"other","query":"x","limit":1})),
            keyed(&f, "bad")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(backend.hits.load(Ordering::SeqCst), before);
    let (code, pages) = request(
        &f,
        "POST",
        &actions,
        Some(query.clone()),
        keyed(&f, "search"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{pages}");
    let page = &pages["data"]["pages"][0]["handle"];
    let (code, body) = request(
        &f,
        "POST",
        &actions,
        Some(json!({"action":"read_page","space":"ops","page":page})),
        keyed(&f, "open"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["text"], "Visible page text[mention]\n");
    let children = request(
        &f,
        "POST",
        &actions,
        Some(json!({"action":"list_children","space":"ops","page":page,"limit":2})),
        keyed(&f, "children"),
    )
    .await
    .1;
    assert_eq!(children["data"]["children"][0]["id"], "12");
    let acquire = json!({"action":"read_attachment","space":"ops","attachment":body["data"]["attachments"][0]["handle"]});
    let (code, file) = request(
        &f,
        "POST",
        &actions,
        Some(acquire.clone()),
        keyed(&f, "attach"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{file}");
    let artifact = file["data"]["artifact_id"]
        .as_str()
        .expect("quarantined artifact");
    assert_eq!(file["data"]["kind"], "quarantined_artifact");
    assert_eq!(file["data"]["size_bytes"], fixtures::SIZE.to_string());
    let physical = private.path().join(format!("quarantine-{artifact}"));
    assert_eq!(
        std::fs::metadata(&physical)
            .expect("actual quarantined file")
            .len(),
        fixtures::SIZE as u64
    );
    assert_eq!(backend.media.load(Ordering::SeqCst), 1);
    assert_eq!(
        request(
            &f,
            "POST",
            &actions,
            Some(acquire.clone()),
            keyed(&f, "attach")
        )
        .await
        .1,
        file
    );
    assert_eq!(
        backend.media.load(Ordering::SeqCst),
        1,
        "idempotent receipt retrieval"
    );
    let code = request(
        &f,
        "POST",
        &actions,
        Some(acquire.clone()),
        keyed(&f, "capacity"),
    )
    .await
    .0;
    assert_eq!(code, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(backend.media.load(Ordering::SeqCst), 1);
    let exposed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT FROM artifacts WHERE artifact_id=$1)")
            .bind(artifact)
            .fetch_one(&f.pool)
            .await
            .expect("user namespace");
    assert!(!exposed);
    let content = format!("{granted}/artifacts/{artifact}/content");
    let (status, bytes) = download(&f, &content, "download").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes.len(), fixtures::SIZE);
    assert_eq!(file["data"]["sha256"], hex::encode(Sha256::digest(bytes)));
    assert_eq!(
        backend.media.load(Ordering::SeqCst),
        1,
        "delivery reads private store"
    );
    backend.moved.store(true, Ordering::SeqCst);
    assert_eq!(
        download(&f, &content, "moved").await.0,
        StatusCode::CONFLICT
    );
    backend.moved.store(false, Ordering::SeqCst);
    assert_eq!(
        download(&f, &content, "restored").await.0,
        StatusCode::NOT_FOUND
    );
    store.collect(20).await.expect("purge changed source");
    assert!(!physical.exists());
    backend.hold_media.store(true, Ordering::SeqCst);
    let in_flight = request(
        &f,
        "POST",
        &actions,
        Some(acquire),
        keyed(&f, "cancel-in-flight"),
    );
    let revoking = async {
        tokio::time::timeout(Duration::from_secs(10), backend.media_reached.notified())
            .await
            .expect("media fetch reached");
        assert_eq!(
            request(&f, "DELETE", &granted, None, keyed(&f, "revoke"))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        backend.media_release.notify_one();
    };
    let ((code, denial), ()) = tokio::join!(in_flight, revoking);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(denial.get("data").is_none());
    store.collect(20).await.expect("reclaim revoked staging");
    store.collect(20).await.expect("idempotent reclaim");
    let records=sqlx::query("SELECT count(*) AS count, count(*) FILTER(WHERE state='purged') AS purged FROM confluence_quarantine WHERE run_id=$1").bind(&run).fetch_one(&f.pool).await.expect("retained receipts");
    assert_eq!(records.get::<i64, _>("count"), 2);
    assert_eq!(records.get::<i64, _>("purged"), 2);
    let usage:(i64,i64)=sqlx::query_as("SELECT used_bytes,records FROM artifact_capacity WHERE root_id=(SELECT root_id FROM confluence_quarantine WHERE artifact_id=$1)").bind(artifact).fetch_one(&f.pool).await.expect("capacity");
    assert_eq!(usage, (0, 0));
    let second = request(&f, "POST", &base, Some(terms), keyed(&f, "again"))
        .await
        .1;
    assert_ne!(second, g);
    assert_eq!(
        download(&f, &content, "revoked").await.0,
        StatusCode::FORBIDDEN
    );
}
async fn download(f: &Fixture, path: &str, key: &str) -> (StatusCode, Vec<u8>) {
    let mut req = Request::post(path);
    *req.headers_mut().expect("headers") = keyed(f, key);
    let res = f
        .router
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("route");
    let code = res.status();
    assert_eq!(res.headers()["cache-control"], "no-store");
    if code == StatusCode::OK {
        assert_eq!(res.headers()["content-type"], "application/octet-stream");
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
        assert_eq!(res.headers()["content-disposition"], "attachment");
    }
    (
        code,
        to_bytes(res.into_body(), 20 * 1024 * 1024)
            .await
            .expect("bounded download")
            .to_vec(),
    )
}
