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
