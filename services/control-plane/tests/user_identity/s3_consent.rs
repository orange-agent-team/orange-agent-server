use super::connector_run::{active_run, keyed};
use super::*;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::time::Duration;
async fn content(f: &Fixture, path: &str, key: &str) -> (StatusCode, Vec<u8>) {
    let mut req = Request::post(path);
    *req.headers_mut().expect("request") = keyed(f, key);
    let response = f
        .router
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("content");
    assert_eq!(response.headers()["cache-control"], "no-store");
    if response.status() == StatusCode::OK {
        assert_eq!(
            response.headers()["content-type"],
            "application/octet-stream"
        );
        assert_eq!(response.headers()["content-disposition"], "attachment");
    }
    (
        response.status(),
        to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .expect("bounded content")
            .to_vec(),
    )
}
#[tokio::test]
async fn s3_receipts_and_delivery_require_the_original_live_authority() {
    let mut f = fixture().await;
    let other = fixture().await;
    let mut cfg: Value = serde_json::from_str(
        &std::env::var("ORANGE_S3_SOURCE_FIXTURE").expect("private source config"),
    )
    .expect("config");
    cfg["owner_id"] = json!(f.user);
    let mut file = tempfile::NamedTempFile::new().expect("private file");
    file.write_all(&serde_json::to_vec(&[&cfg]).expect("config bytes"))
        .expect("private configuration");
    f.identity.s3_sources =
        orange_control_plane::s3_sources::Sources::load_development_file(file.path())
            .expect("registered S3 source");
    f.router = auth::routes::router(f.identity.clone());
    let (run, _root) = active_run(&f).await;
    if let Ok(driver) = std::env::var("ORANGE_CN06_BROWSER") {
        let receipt = super::connector_run::browser(&f, &run, driver).await;
        assert_eq!(
            receipt,
            json!({"listed":true,"inspected":true,"downloaded_bytes":3145728,"sha256":hex::encode(Sha256::digest(vec![0u8;3145728])),"revoked_after_reload":true})
        );
    }
    let path = format!("/api/v1/agent/runs/{run}/connectors/s3");
    assert_eq!(
        request(&other, "GET", &path, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let (code, inventory) = request(&f, "GET", &path, None, headers(&f)).await;
    assert_eq!(code, StatusCode::OK);
    let src = &inventory["sources"][0];
    assert_eq!(src["prefix"], "team/approved");
    let terms = json!({"source_id":src["source_id"],"source_digest":src["source_digest"],"duration_seconds":180});
    let (code, granted) = request(&f, "POST", &path, Some(terms.clone()), keyed(&f, "grant")).await;
    assert_eq!(code, StatusCode::OK, "{granted}");
    let id = granted["grant_id"].as_str().expect("grant ID");
    let grant = format!("{path}/{id}");
    let actions = format!("{grant}/actions");
    let (code, list) = request(
        &f,
        "POST",
        &actions,
        Some(json!({"action":"list_objects","limit":2})),
        keyed(&f, "list"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(list["data"]["keys"], json!(["a.txt", "b.txt"]));
    let (code, head) = request(
        &f,
        "POST",
        &actions,
        Some(json!({"action":"head_object","key":"medium.bin"})),
        keyed(&f, "head"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{head}");
    assert_eq!(head["data"]["size_bytes"], "3145728");
    let reading = json!({"action":"read_object","expected":head["data"]});
    let (code, read) = request(
        &f,
        "POST",
        &actions,
        Some(reading.clone()),
        keyed(&f, "read"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{read}");
    let receipt = &read["data"];
    assert_eq!(receipt["kind"], "source_backed_artifact");
    assert_eq!(
        request(&f, "POST", &actions, Some(reading), keyed(&f, "read"))
            .await
            .1,
        read
    );
    assert_eq!(
        request(
            &f,
            "POST",
            &actions,
            Some(json!({"action":"list_objects","limit":2})),
            keyed(&f, "read")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let download = format!(
        "{grant}/artifacts/{}/content",
        receipt["artifact_id"].as_str().expect("reference")
    );
    let (code, bytes) = content(&f, &download, "exact-bytes").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(bytes.len(), 3 * 1024 * 1024);
    assert_eq!(receipt["sha256"], hex::encode(Sha256::digest(&bytes)));
    let fence = std::path::PathBuf::from(std::env::var("ORANGE_CN06_FENCE").expect("gate"));
    std::fs::write(fence.join("arm-revoke"), b"1").expect("arm one GET");
    let revoker = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !fence.join("upstream-read").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("real upstream bytes");
        assert_eq!(
            request(&f, "DELETE", &grant, None, keyed(&f, "revoke"))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        std::fs::write(fence.join("release-get"), b"1").expect("finish upstream transfer");
    };
    let ((code, denial), ()) = tokio::join!(content(&f, &download, "inflight"), revoker);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(denial.len() < 4096);
    let (code, regrant) = request(&f, "POST", &path, Some(terms), keyed(&f, "new-grant")).await;
    assert_eq!(code, StatusCode::OK);
    assert_ne!(regrant, granted);
    assert_eq!(content(&f, &download, "old").await.0, StatusCode::FORBIDDEN);
    let wrong = download.replace(id, regrant["grant_id"].as_str().expect("second grant"));
    assert_eq!(content(&f, &wrong, "wrong").await.0, StatusCode::NOT_FOUND);
    // One incomplete S3 call plus three active legacy PG reservations occupy the same Run.
    let new_id = regrant["grant_id"].as_str().expect("new grant");
    let new_actions = format!("{path}/{new_id}/actions");
    sqlx::query("INSERT INTO pg_view_grants(grant_id,run_id,source_id,source_digest,view_ids,request_key,request_digest,expires_at) VALUES($1,$2,'pg-fixture',repeat('a',64),ARRAY['v'],'quota',repeat('b',64),clock_timestamp()+interval '3 minutes')")
        .bind(&run).bind(&run).execute(&f.pool).await.expect("legacy PG quota fixture");
    sqlx::query("INSERT INTO pg_view_calls(run_id,request_key,grant_id,until_at) SELECT $1,'pg-active-'||i,$1,clock_timestamp()+interval '30 seconds' FROM generate_series(1,3) i").bind(&run).execute(&f.pool).await.expect("three PG reservations");
    let request_list = json!({"action":"list_objects","limit":1});
    assert_eq!(
        request(
            &f,
            "POST",
            &new_actions,
            Some(request_list.clone()),
            keyed(&f, "shared-busy")
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("release PG slots");
    sqlx::query("INSERT INTO pg_view_calls(run_id,request_key,grant_id,until_at,finished_at) SELECT $1,'past-'||i,$1,clock_timestamp(),clock_timestamp() FROM generate_series(1,99-(SELECT count(*) FROM connector_run_calls WHERE run_id=$1)) i").bind(&run).execute(&f.pool).await.expect("99 mixed calls");
    assert_eq!(
        request(
            &f,
            "POST",
            &new_actions,
            Some(request_list.clone()),
            keyed(&f, "mixed-100")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &f,
            "POST",
            &new_actions,
            Some(request_list),
            keyed(&f, "mixed-101")
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A signing refusal must survive a fresh in-memory registry and must not disable another revision.
    let mut recovery = fixture().await;
    let (run2, _root2) = active_run(&recovery).await;
    cfg["owner_id"] = json!(recovery.user);
    let mut corrupted = cfg.clone();
    corrupted["secret_key"] = json!("retired-test-key");
    fn register(f: &mut Fixture, cfg: &Value) {
        let mut file = tempfile::NamedTempFile::new().expect("private config");
        file.write_all(&serde_json::to_vec(&[cfg]).expect("json"))
            .expect("config");
        f.identity.s3_sources =
            orange_control_plane::s3_sources::Sources::load_development_file(file.path())
                .expect("loaded");
        f.router = auth::routes::router(f.identity.clone());
    }
    register(&mut recovery, &corrupted);
    let base = format!("/api/v1/agent/runs/{run2}/connectors/s3");
    let inventory = request(&recovery, "GET", &base, None, headers(&recovery))
        .await
        .1;
    let terms0 = json!({"source_id":"documents","source_digest":inventory["sources"][0]["source_digest"],"duration_seconds":180});
    let grant0 = request(
        &recovery,
        "POST",
        &base,
        Some(terms0.clone()),
        keyed(&recovery, "fault-grant"),
    )
    .await
    .1;
    let actions0 = format!(
        "{base}/{}/actions",
        grant0["grant_id"].as_str().expect("grant")
    );
    let listing = json!({"action":"list_objects","limit":1});
    assert_eq!(
        request(
            &recovery,
            "POST",
            &actions0,
            Some(listing.clone()),
            keyed(&recovery, "fault")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    register(&mut recovery, &cfg);
    let inventory = request(&recovery, "GET", &base, None, headers(&recovery))
        .await
        .1;
    assert_eq!(inventory["sources"], json!([]));
    assert_eq!(
        request(
            &recovery,
            "POST",
            &actions0,
            Some(listing.clone()),
            keyed(&recovery, "after-reload")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(
            &recovery,
            "POST",
            &base,
            Some(terms0.clone()),
            keyed(&recovery, "stale-regrant")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM s3_source_calls WHERE run_id=$1")
        .bind(&run2)
        .fetch_one(&recovery.pool)
        .await
        .expect("source dispatches");
    assert_eq!(used, 1);
    cfg["credential_revision"] = json!("rotated-v2");
    register(&mut recovery, &cfg);
    let inventory = request(&recovery, "GET", &base, None, headers(&recovery))
        .await
        .1;
    let mut terms2 = terms0.clone();
    terms2["source_digest"] = inventory["sources"][0]["source_digest"].clone();
    assert_ne!(terms0, terms2);
    let grant2 = request(
        &recovery,
        "POST",
        &base,
        Some(terms2),
        keyed(&recovery, "rotated-grant"),
    )
    .await
    .1;
    let actions2 = format!(
        "{base}/{}/actions",
        grant2["grant_id"].as_str().expect("new grant")
    );
    assert_eq!(
        request(
            &recovery,
            "POST",
            &actions2,
            Some(listing),
            keyed(&recovery, "rotated")
        )
        .await
        .0,
        StatusCode::OK
    );
}
