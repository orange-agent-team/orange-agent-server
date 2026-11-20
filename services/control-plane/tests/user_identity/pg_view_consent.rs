use super::*;
use orange_control_plane::{artifacts::ArtifactStore, pg_view_sources::Sources};
use orange_protocol::artifacts::CreateUpload;
use sha2::{Digest, Sha256};
use std::io::Write;

fn keyed(f: &Fixture, k: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", k.parse().expect("key"));
    h
}

#[tokio::test]
async fn scoped_source_reads_require_live_run_and_grant_at_delivery() {
    let mut f = fixture().await;
    let other = fixture().await;
    let source =
        sqlx::PgPool::connect(&std::env::var("ORANGE_VIEW_FIXTURE_ADMIN").expect("fixture"))
            .await
            .expect("source pool");
    let mut config: Value =
        serde_json::from_str(&std::env::var("ORANGE_VIEW_FIXTURE_CONFIG").expect("fixture config"))
            .expect("source config");
    let def: String = sqlx::query_scalar("SELECT pg_get_viewdef('source.sales'::regclass,false)")
        .fetch_one(&source)
        .await
        .expect("definition");
    config["registration"]["views"]["sales"]["definition_sha256"] =
        json!(hex::encode(Sha256::digest(def)));
    config["registration"]["views"]["sales"]["relation_oid"] = json!(
        sqlx::query_scalar::<_, i64>("SELECT 'source.sales'::regclass::oid::bigint")
            .fetch_one(&source)
            .await
            .expect("view OID")
    );
    config["owner_id"] = json!(f.user);
    let mut file = tempfile::NamedTempFile::new().expect("private source file");
    file.write_all(&serde_json::to_vec(&[&config]).expect("fixture bytes"))
        .expect("write file");
    f.identity.pg_sources = Sources::load_development_file(file.path()).expect("registered source");
    f.router = auth::routes::router(f.identity.clone());
    let root = tempfile::tempdir().expect("object root");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    let store = ArtifactStore::open_local(f.pool.clone(), root.path(), 1048576, 100)
        .await
        .expect("object store");
    let actor = f
        .identity
        .artifact_actor(&headers(&f), true)
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
        &f,
        "POST",
        "/api/v1/account/budgets",
        Some(json!({"scope_id":"views","limit":"100"})),
        keyed(&f, "budget"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let run = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO cloud_runs(run_id,owner_id,user_id,request_key,request_digest,root_budget_id,input_artifact_id,input_version_id,status) VALUES($1,$2,$2,'test',$3,$4,$5,$6,'running')").bind(&run).bind(&f.user).bind("a".repeat(64)).bind(budget["root_budget_id"].as_str().expect("budget")).bind(input.artifact_id).bind(input.version_id).execute(&f.pool).await.expect("fixture running Run");
    if let Ok(driver) = std::env::var("ORANGE_CN05_BROWSER") {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let addr = listener.local_addr().expect("address");
        let app = f.router.clone();
        let serving =
            tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test API") });
        let dir =
            std::path::PathBuf::from(std::env::var("ORANGE_CN05_EVIDENCE").expect("evidence"));
        let mut child =
            tokio::process::Command::new(std::env::var("ORANGE_CN05_NODE").expect("node"))
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
        assert_eq!(
            receipt,
            json!({"approved":true,"listed":true,"described":true,"queried":true,"revoked_after_reload":true})
        );
        std::fs::write(dir.join("browser.json"), out.stdout).expect("save receipt");
    }
    let base = format!("/api/v1/agent/runs/{run}/connectors/postgres");
    assert_eq!(
        request(&other, "GET", &base, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let (code, list) = request(&f, "GET", &base, None, headers(&f)).await;
    assert_eq!(code, StatusCode::OK);
    let terms = json!({"source_id":"sales-db","source_digest":list["sources"][0]["source_digest"],"view_ids":["sales"],"duration_seconds":60});
    let (code, g) = request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve")).await;
    assert_eq!(code, StatusCode::OK, "{g}");
    assert_eq!(
        request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve"))
            .await
            .1,
        g
    );
    let grant = format!("{base}/{}", g["grant_id"].as_str().expect("grant id"));
    let actions = format!("{grant}/actions");
    let query = json!({"action":"query_view","view_id":"sales","columns":["seq"],"limit":1});
    let (code, rows) = request(&f, "POST", &actions, Some(query.clone()), keyed(&f, "read")).await;
    assert_eq!(code, StatusCode::OK, "{rows}");
    assert_eq!(rows["data"]["rows"], json!([{"seq":"10"}]));
    assert_eq!(
        request(&f, "POST", &actions, Some(query.clone()), keyed(&f, "read"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut lock = source.begin().await.expect("source fence");
    sqlx::query("LOCK source.sales IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("fence");
    let reading = request(
        &f,
        "POST",
        &actions,
        Some(query.clone()),
        keyed(&f, "inflight"),
    );
    let revoking = async {
        tokio::time::timeout(std::time::Duration::from_secs(3),async{
            loop{
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE usename=$1 AND wait_event_type='Lock')").bind(config["registration"]["read_role"].as_str().expect("role")).fetch_one(&source).await.expect("check wait");
                if waiting{break;}tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("connector reached source");
        assert_eq!(
            request(&f, "DELETE", &grant, None, headers(&f)).await.0,
            StatusCode::NO_CONTENT
        );
        lock.commit().await.expect("release");
    };
    let ((code, denial), ()) = tokio::join!(reading, revoking);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(denial.get("data").is_none());
    let (code, new_grant) =
        request(&f, "POST", &base, Some(terms), keyed(&f, "approve-again")).await;
    assert_eq!(code, StatusCode::OK);
    assert_ne!(new_grant, g);
    assert_eq!(
        request(&f, "POST", &actions, Some(query), keyed(&f, "old"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let new_id = new_grant["grant_id"].as_str().expect("new grant");
    let current = format!("{base}/{new_id}/actions");
    // Set up a real occupied slot window and then the exact final quota boundary.
    sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("end old window");
    sqlx::query("INSERT INTO pg_view_calls(run_id,grant_id,request_key,until_at) SELECT $1,$2,'busy-'||i,clock_timestamp()+interval '30 seconds' FROM generate_series(1,4) i").bind(&run).bind(new_id).execute(&f.pool).await.expect("four active slots");
    let list = json!({"action":"list_views"});
    assert_eq!(
        request(&f, "POST", &current, Some(list.clone()), keyed(&f, "fifth"))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("release slots");
    sqlx::query("INSERT INTO pg_view_calls(run_id,grant_id,request_key,until_at,finished_at) SELECT $1,$2,'past-'||i,clock_timestamp(),clock_timestamp() FROM generate_series(1,99-(SELECT count(*) FROM pg_view_calls WHERE run_id=$1)) i").bind(&run).bind(new_id).execute(&f.pool).await.expect("99 calls");
    assert_eq!(
        request(
            &f,
            "POST",
            &current,
            Some(list.clone()),
            keyed(&f, "hundredth")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&f, "POST", &current, Some(list), keyed(&f, "over"))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    source.close().await;
}
