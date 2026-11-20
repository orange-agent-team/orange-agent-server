use super::*;
use orange_control_plane::artifacts::ArtifactStore;
use orange_protocol::artifacts::CreateUpload;
use sha2::{Digest, Sha256};

pub(super) fn keyed(f: &Fixture, key: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", key.parse().expect("fixture key"));
    h
}
pub(super) async fn active_run(f: &Fixture) -> (String, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("object root");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    let store = ArtifactStore::open_local(f.pool.clone(), root.path(), 1048576, 100)
        .await
        .expect("object store");
    let actor = f
        .identity
        .artifact_actor(&headers(f), true)
        .await
        .expect("actor");
    let upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        size_bytes: "1".into(),
        media_type: "text/plain".into(),
        sha256: hex::encode(Sha256::digest(b"x")),
    };
    store
        .begin_upload(&actor, &f.user, &upload)
        .await
        .expect("reserve");
    let input = store
        .put(
            actor,
            f.user.clone(),
            upload.upload_id,
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(vec![b'x']))]),
        )
        .await
        .expect("publish");
    let (code, budget) = request(
        f,
        "POST",
        "/api/v1/account/budgets",
        Some(json!({"scope_id":"views","limit":"100"})),
        keyed(f, "budget"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let run = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO cloud_runs(run_id,owner_id,user_id,request_key,request_digest,root_budget_id,input_artifact_id,input_version_id,status) VALUES($1,$2,$2,'test',$3,$4,$5,$6,'running')").bind(&run).bind(&f.user).bind("a".repeat(64)).bind(budget["root_budget_id"].as_str().expect("budget")).bind(input.artifact_id).bind(input.version_id).execute(&f.pool).await.expect("fixture running Run");
    (run, root)
}

pub(super) async fn browser(f: &Fixture, run: &str, driver: String) -> Value {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let addr = listener.local_addr().expect("address");
    let app = f.router.clone();
    let serving =
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test API") });
    let dir =
        std::path::PathBuf::from(std::env::var("ORANGE_CONNECTOR_EVIDENCE").expect("evidence"));
    let mut child =
        tokio::process::Command::new(std::env::var("ORANGE_CONNECTOR_NODE").expect("node"))
            .arg(driver)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("Chrome driver");
    let packet = json!({"api":format!("http://{addr}"),"origin":f.identity.config.user_origin,"token":f.token,"run":run,"screenshot":dir.join("rows.png")});
    let mut input = child.stdin.take().expect("private pipe");
    input
        .write_all(&serde_json::to_vec(&packet).expect("JSON"))
        .await
        .expect("send private fixture");
    drop(input);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .expect("driver deadline")
    .expect("browser");
    serving.abort();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: Value = serde_json::from_slice(&out.stdout).expect("browser receipt");
    std::fs::write(dir.join("browser.json"), out.stdout).expect("save browser receipt");
    receipt
}

#[cfg(any(feature = "repository-fixture-tests", feature = "notion-fixture-tests"))]
pub(super) async fn opted_in(f: &Fixture, legacy: &str, flag: &str) -> String {
    use sqlx::Row;
    // Admission uses the real create handler. Only the platform Host's readiness is a fixture here.
    let input = sqlx::query(
        "SELECT root_budget_id,input_artifact_id,input_version_id FROM cloud_runs WHERE run_id=$1",
    )
    .bind(legacy)
    .fetch_one(&f.pool)
    .await
    .expect("sealed input");
    sqlx::query("INSERT INTO cloud_pool_hosts(host_instance_id,certificate_sha256,profile_id,attestation_digest,region_id,max_workspaces,enabled,credential_expires_at,accepting,live_until,input_protocol_version) VALUES($1,$2,'dev-gvisor-linux-arm64-v1',$3,'dev',8,true,clock_timestamp()+interval '10 minutes',true,clock_timestamp()+interval '10 minutes',1)").bind(Uuid::new_v4().to_string()).bind(hex::encode(Sha256::digest(format!("{legacy}/{flag}").as_bytes()))).bind("1".repeat(64)).execute(&f.pool).await.expect("registered host fixture");
    let mut create = json!({"root_budget_id":input.get::<String,_>("root_budget_id"),"input_artifact_id":input.get::<String,_>("input_artifact_id"),"input_version_id":input.get::<String,_>("input_version_id"),"resource_profile_id":"dev-gvisor-linux-arm64-v1","workspace_ttl_seconds":300});
    create[flag] = json!(true);
    let (code, result) = request(
        f,
        "POST",
        "/api/v1/agent/runs",
        Some(create.clone()),
        keyed(f, flag),
    )
    .await;
    assert!(
        matches!(code, StatusCode::OK | StatusCode::CREATED),
        "{result}"
    );
    let run = result["run_id"].as_str().expect("created run");
    create[flag] = json!(false);
    assert_eq!(
        request(
            f,
            "POST",
            "/api/v1/agent/runs",
            Some(create),
            keyed(f, flag)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE cloud_runs SET status='running',version=version+1 WHERE run_id=$1")
        .bind(run)
        .execute(&f.pool)
        .await
        .expect("ready Run fixture");

    run.to_owned()
}
