//! Black-box test of the real development platform Host binary and Linux OCI backend.
use super::*;
use sqlx::Row;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

fn launch(config: &FsPath) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orange-cloud-host-node"));
    cmd.env_clear();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    cmd.env("ORANGE_ENV", "development")
        .env("ORANGE_CLOUD_HOST_CONFIG", config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("real Host process")
}
async fn wait_for(f: &Fixture, path: &str, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let (status, row) = request(f, "GET", path, None, headers(f)).await;
            assert_eq!(status, StatusCode::OK, "{row}");
            if (kind == "capacity" && row["can_reserve"] == true)
                || row["workspace"]["status"] == kind
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("timely real Host lifecycle transition")
}
pub(super) struct Setup<'a> {
    pub tls: &'a FsPath,
    pub port: u16,
    pub id: &'a str,
    pub proof: &'a str,
}
pub(super) async fn exercise(
    f: &Fixture,
    store: &ArtifactStore,
    body: &Value,
    setup: Setup<'_>,
    evidence: &FsPath,
) {
    let Setup {
        tls,
        port,
        id,
        proof,
    } = setup;
    let work = tempfile::tempdir().expect("private operator directory");
    let repo = FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("server root");
    let python = PathBuf::from(std::env::var_os("ORANGE_CLOUD_QA_PYTHON").expect("Python"));
    let journal = work.path().join("journal");
    let config = work.path().join("config.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"endpoint":format!("https://127.0.0.1:{port}"),"host_instance_id":id,"attestation_digest":proof,
        "ca":tls.join("host-ca.pem"),"certificate":tls.join("host.pem"),"private_key":tls.join("host.key"),"journal_dir":journal,
        "driver_root":repo,"archive":repo.join(".local/gvisor-download/gvisor-aarch64.tar.bz2"),"python":python})).expect("config")).expect("private config file");
    let mut child = launch(&config);
    let mut receipts = Vec::new();
    for cause in ["user_cancel", "controller_restart", "input_deleted"] {
        let terminate = cause == "controller_restart";
        wait_for(f, "/api/v1/agent/workspace-capacity", "capacity").await;
        let response = request(f, "POST", API, Some(body.clone()), keyed(f, cause)).await;
        assert_eq!(response.0, StatusCode::CREATED, "{}", response.1);
        let run = response.1["run_id"].as_str().expect("run");
        let path = format!("{API}/{run}");
        let ready = wait_for(f, &path, "ready").await;
        assert_eq!(ready["status"], "queued");
        let workspace = ready["workspace"]["workspace_id"]
            .as_str()
            .expect("workspace");
        let saved = journal.join(format!("{workspace}.json"));
        let record = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let entry: Value =
                    serde_json::from_slice(&std::fs::read(&saved).expect("Host journal"))
                        .expect("metadata");
                if entry["phase"] == "ready" {
                    return entry;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("durable readiness");
        let allocation = record["allocation"].as_str().expect("concrete allocation");
        let digest:String=sqlx::query_scalar("SELECT materialization_digest FROM cloud_workspace_ready_receipts WHERE allocation_id=$1")
            .bind(allocation).fetch_one(&f.pool).await.expect("registered actual readiness");
        assert_eq!(record["readiness_digest"], digest);
        if terminate {
            child
                .kill()
                .await
                .expect("terminate only the test's Host process");
            child.wait().await.expect("reap original Host");
            child = launch(&config);
        } else if cause == "input_deleted" {
            let upload: String = sqlx::query_scalar(
                "SELECT upload_id FROM artifact_uploads WHERE owner_id=$1 AND artifact_id=$2",
            )
            .bind(&f.user)
            .bind(body["input_artifact_id"].as_str().expect("exact source"))
            .fetch_one(&f.pool)
            .await
            .expect("owned source upload");
            let actor = f
                .identity
                .artifact_actor(&headers(f), true)
                .await
                .expect("current user");
            store
                .delete(&actor, &f.user, &upload)
                .await
                .expect("delete actual staged source");
        } else {
            let cancelled = request(f, "DELETE", &path, None, headers(f)).await;
            assert_eq!(cancelled.1["status"], "cancelled");
            assert_eq!(cancelled.1["workspace"]["status"], "draining");
        }
        let end = wait_for(f, &path, "deleted").await;
        assert_eq!(
            end["status"],
            if cause != "user_cancel" {
                "recovery_required"
            } else {
                "cancelled"
            }
        );
        let out = Command::new(&python)
            .arg(repo.join("scripts/cloud-workspace-driver.py"))
            .arg("recover")
            .arg(allocation)
            .output()
            .await
            .expect("query exact guest receipt");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let outcome: Value = serde_json::from_slice(&out.stdout).expect("guest receipt");
        assert_eq!(outcome["allocation_id"], allocation);
        assert_eq!(outcome["input_staged"], true);
        assert_eq!(outcome["phase"], "cleaned");
        for key in ["sandbox_absent", "cgroup_absent", "files_absent"] {
            assert_eq!(outcome[key], true);
        }
        let db=sqlx::query("SELECT receipt_id,evidence_digest FROM cloud_workspace_cleanup_receipts WHERE allocation_id=$1")
            .bind(allocation).fetch_one(&f.pool).await.expect("actual cleanup acknowledged");
        let computed = hex::encode(Sha256::digest(
            serde_json::to_vec(&outcome).expect("canonical receipt"),
        ));
        assert_eq!(db.get::<String, _>("evidence_digest"), computed);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM cloud_workspace_claims WHERE workspace_id=$1"
            )
            .bind(workspace)
            .fetch_one(&f.pool)
            .await
            .expect("only one allocation"),
            1
        );
        let mut evidence = outcome;
        evidence["recovered_after_controller_exit"] = json!(terminate);
        evidence["cause"] = json!(cause);
        evidence["control_plane_cleanup_receipt"] = json!(db.get::<String, _>("receipt_id"));
        receipts.push(evidence);
    }
    if let Ok(driver) = std::env::var("ORANGE_CLOUD_MANAGED_BROWSER") {
        wait_for(f, "/api/v1/agent/workspace-capacity", "capacity").await;
        let result = web(f, store, evidence, &driver).await;
        let row=sqlx::query("SELECT c.allocation_id,rr.materialization_digest,cleanup.evidence_digest FROM cloud_workspace_claims c
            JOIN cloud_workspace_ready_receipts rr USING(allocation_id) JOIN cloud_workspace_cleanup_receipts cleanup USING(allocation_id)
            JOIN cloud_workspaces w ON w.workspace_id=c.workspace_id WHERE w.workspace_id=$1 AND w.run_id=$2 AND w.status='deleted'")
            .bind(result["workspace_id"].as_str().expect("browser workspace")).bind(result["run_id"].as_str().expect("browser Run")).fetch_one(&f.pool).await.expect("actual materialization and cleanup");
        let allocation: String = row.get("allocation_id");
        let output = Command::new(&python)
            .arg(repo.join("scripts/cloud-workspace-driver.py"))
            .arg("recover")
            .arg(&allocation)
            .output()
            .await
            .expect("recover observed allocation");
        assert!(output.status.success());
        let proof: Value = serde_json::from_slice(&output.stdout).expect("actual guest proof");
        assert_eq!(
            proof["input_sha256"],
            hex::encode(Sha256::digest(b"Prepare this exact real sandbox input."))
        );
        assert_eq!(proof["phase"], "cleaned");
        assert_eq!(
            row.get::<String, _>("evidence_digest"),
            hex::encode(Sha256::digest(
                serde_json::to_vec(&proof).expect("canonical guest receipt")
            ))
        );
        std::fs::write(evidence.join("managed-browser.json"),serde_json::to_vec_pretty(&json!({"browser":result,"guest":proof,"materialization_digest":row.get::<String,_>("materialization_digest")})).expect("receipt")).expect("store verified browser evidence");
    }
    let pid = child.id().expect("owned current Host").to_string();
    let signal = Command::new("/bin/kill")
        .args(["-INT", &pid])
        .status()
        .await
        .expect("stop own Host");
    assert!(signal.success());
    assert!(
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("graceful stop deadline")
            .expect("stop")
            .success()
    );
    std::fs::write(
        evidence.join("managed-host.json"),
        serde_json::to_vec_pretty(&receipts).expect("reports"),
    )
    .expect("store host evidence");
}

async fn web(f: &Fixture, store: &ArtifactStore, folder: &FsPath, driver: &str) -> Value {
    use tokio::io::AsyncWriteExt;
    let app = f
        .router
        .clone()
        .merge(orange_control_plane::artifacts::http::router(
            f.identity.clone(),
            store.clone(),
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("private user API");
    let addr = listener.local_addr().expect("actual port");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let mut driver = Command::new(std::env::var("ORANGE_CLOUD_WEB_NODE").expect("pinned node"))
        .arg(driver)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("real browser");
    let packet = json!({"api":format!("http://{addr}"),"token":f.token,"user":f.user,"ready":folder.join("workspace-ready.png"),"closed":folder.join("workspace-deleted.png")});
    let mut input = driver.stdin.take().expect("private input");
    input
        .write_all(&serde_json::to_vec(&packet).expect("packet"))
        .await
        .expect("feed test session");
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(120), driver.wait_with_output())
        .await
        .expect("bounded managed browser")
        .expect("driver exit");
    stop.send(()).expect("stop API");
    server.await.expect("server task").expect("clean shutdown");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("browser receipt")
}
