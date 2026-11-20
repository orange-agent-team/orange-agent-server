//! A separate authenticated hosted Run shares the existing development model
//! provider and real metering pipeline. Registration is verified in the guest.
use super::*;
use orange_control_plane::{
    artifacts::ArtifactReader,
    cloud_host::{self, HostTls},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{net::SocketAddr, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

pub(super) struct Setup<'a> {
    pub pool: &'a PgPool,
    pub object_root: &'a Path,
    pub api: SocketAddr,
    pub user: &'a str,
    pub owner: &'a str,
    pub budget: &'a str,
    pub price: &'a str,
    pub cookie: &'a str,
}
pub(super) async fn exercise(c: Setup<'_>, driver: &str) {
    let tls =
        PathBuf::from(std::env::var("ORANGE_CLOUD_MODEL_QA_DIR").expect("isolated platform PKI"));
    let evidence =
        PathBuf::from(std::env::var("ORANGE_CLOUD_MODEL_EVIDENCE").expect("owned evidence"));
    let python = PathBuf::from(std::env::var("ORANGE_CLOUD_QA_PYTHON").expect("Python"));
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo");
    let host_id = Uuid::new_v4().to_string();
    let admission = Command::new(&python)
        .arg(repo.join("scripts/admit-local-cloud-host.py"))
        .arg("--database-url-file")
        .arg(tls.join("database-url"))
        .arg("--host-cert")
        .arg(tls.join("host.pem"))
        .arg("--platform-host-ca")
        .arg(tls.join("host-ca.pem"))
        .arg("--host-id")
        .arg(&host_id)
        .output()
        .await
        .expect("qualify and register the actual development Host");
    assert!(
        admission.status.success(),
        "{}",
        String::from_utf8_lossy(&admission.stderr)
    );
    let report: Value = serde_json::from_str(
        std::str::from_utf8(&admission.stdout)
            .expect("admission output")
            .lines()
            .last()
            .expect("receipt"),
    )
    .expect("admission JSON");
    let reader = ArtifactReader::open_local(c.pool.clone(), c.object_root)
        .await
        .expect("registered read-only artifact root");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("cloud gateway");
    let port = listener.local_addr().expect("addr").port();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let creds = HostTls {
        server_certificate_pem: std::fs::read(tls.join("gateway.pem")).expect("cert"),
        server_private_key_pem: std::fs::read(tls.join("gateway.key")).expect("key"),
        platform_host_ca_pem: std::fs::read(tls.join("host-ca.pem")).expect("separate CA"),
    };
    let gateway = tokio::spawn(cloud_host::serve_with_storage(
        listener,
        c.pool.clone(),
        creds,
        Some(reader.clone()),
        async {
            let _ = stopped.await;
        },
    ));
    let private = runtime();
    let cfg = private.path().join("node.json");
    std::fs::write(&cfg,serde_json::to_vec(&json!({"endpoint":format!("https://127.0.0.1:{port}"),"host_instance_id":host_id,"attestation_digest":report["attestation_digest"],
        "ca":tls.join("host-ca.pem"),"certificate":tls.join("host.pem"),"private_key":tls.join("host.key"),"journal_dir":private.path().join("journal"),
        "driver_root":repo,"archive":repo.join(".local/gvisor-download/gvisor-aarch64.tar.bz2"),"python":python})).expect("config")).expect("private node config");
    let mut command = Command::new(env!("CARGO_BIN_EXE_orange-cloud-host-node"));
    command.env_clear();
    for k in ["PATH", "HOME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(k) {
            command.env(k, v);
        }
    }
    let mut node = command
        .env("ORANGE_ENV", "development")
        .env("ORANGE_CLOUD_HOST_CONFIG", &cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("real cloud Host process");
    let mut browser = Command::new(std::env::var("ORANGE_WEB_NODE_BINARY").expect("pinned node"))
        .arg(driver)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("hosted model browser");
    let input_file = private.path().join("oracle-input.json");
    let output_file = private.path().join("oracle-output.json");
    let gate_quote = private.path().join("queue-quote");
    let gate_locked = private.path().join("queue-locked");
    let gate_release = private.path().join("queue-release");
    let packet = json!({"api":format!("http://{}/",c.api),"cookie":c.cookie,"user":c.user,"owner":c.owner,"budget":c.budget,"price":c.price,"model":profile().model_version,
        "gate_quote":gate_quote,"gate_locked":gate_locked,"gate_release":gate_release,"oracle_input":input_file,"oracle_output":output_file,"screenshot":evidence.join("actual-hosted-model.png")});
    let mut fd = browser.stdin.take().expect("private stdin");
    fd.write_all(&serde_json::to_vec(&packet).expect("packet"))
        .await
        .expect("supply fixture");
    drop(fd);
    let (finish, mut finished) = tokio::sync::oneshot::channel();
    let wait_browser = async {
        let result =
            tokio::time::timeout(Duration::from_secs(230), browser.wait_with_output()).await;
        let _ = finish.send(());
        result
    };
    let gate = async {
        while !gate_quote.is_file() {
            if !matches!(
                finished.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let quote = std::fs::read_to_string(&gate_quote).expect("explicit second quote");
        let mut tx = c.pool.begin().await.expect("provider gate");
        sqlx::query("SET LOCAL lock_timeout='5s'")
            .execute(&mut *tx)
            .await
            .expect("bounded lock");
        sqlx::query("SELECT n.node_id FROM nodes n JOIN quote_execution_plans ep ON ep.target_node_id=n.node_id JOIN cloud_model_quotes q USING(quote_id)
            JOIN cloud_runs r ON r.run_id=q.run_id WHERE ep.quote_id=$1 AND r.owner_id=$2 AND r.user_id=$3 FOR UPDATE OF n")
            .bind(quote).bind(c.owner).bind(c.user).fetch_one(&mut *tx).await.expect("only the owned provider before start");
        std::fs::write(&gate_locked, b"held").expect("gate ready");
        let observed = tokio::time::timeout(Duration::from_secs(25), async {
            while !gate_release.is_file() {
                if !matches!(
                    finished.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ) {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            true
        })
        .await
        .unwrap_or(false);
        tx.rollback().await.expect("release test provider");
        observed
    };
    let (output, gate_observed) = tokio::join!(wait_browser, gate);
    let output = output
        .expect("bounded model browser")
        .expect("browser exit");
    if !output.status.success() {
        let facts:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('run',r.run_id,'status',r.status,'workspace',w.status,'input_bound',i.job_id IS NOT NULL,
            'job',j.job_id,'execution',j.execution_state,'settlement',j.settlement_state,'commit',c.commit_id IS NOT NULL,'attempt',a.state,
            'response_published',f.run_id IS NOT NULL) FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) LEFT JOIN cloud_model_jobs m USING(run_id)
            LEFT JOIN jobs j ON j.job_id=m.job_id LEFT JOIN job_inputs i ON i.job_id=j.job_id LEFT JOIN job_response_commits c ON c.job_id=j.job_id
            LEFT JOIN worker_execution_attempts a ON a.job_id=j.job_id LEFT JOIN cloud_model_results f ON f.run_id=r.run_id WHERE r.owner_id=$1")
            .bind(c.owner).fetch_all(c.pool).await.expect("failure metadata");
        std::fs::write(
            evidence.join("failed-states.json"),
            serde_json::to_vec_pretty(&facts).expect("facts"),
        )
        .expect("failure diagnosis");
    }
    let pid = node.id().expect("owned Host").to_string();
    let _ = Command::new("/bin/kill")
        .args(["-INT", &pid])
        .status()
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(60), node.wait()).await;
    stop.send(()).expect("stop cloud gateway");
    gateway
        .await
        .expect("gateway task")
        .expect("bounded shutdown");
    reader
        .drain()
        .await
        .expect("close separate read-only handle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(closed.expect("node shutdown").expect("exit").success());
    assert!(
        gate_observed,
        "second queued Job was not cancelled under the owned barrier"
    );
    let browser: Value = serde_json::from_slice(&output.stdout).expect("browser receipt");
    let run = browser["run_id"].as_str().expect("Run");
    let job = browser["job_id"].as_str().expect("Job");
    let row=sqlx::query("SELECT r.status,w.status AS workspace,j.charge,j.execution_state,j.settlement_state,u.input_units,u.output_units,
        q.max_amount,m.input_price,m.output_price,c.allocation_id,cleanup.evidence_digest,settle.transaction_id
        FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_model_jobs link USING(run_id)
        JOIN jobs j ON j.job_id=link.job_id JOIN cloud_model_results f ON f.run_id=r.run_id AND f.job_id=j.job_id
        JOIN cloud_workspace_claims c ON c.workspace_id=w.workspace_id JOIN cloud_workspace_ready_receipts rr USING(allocation_id)
        JOIN cloud_workspace_cleanup_receipts cleanup ON cleanup.allocation_id=c.allocation_id
        JOIN quotes q ON q.quote_id=j.quote_id JOIN quote_metering m ON m.quote_id=q.quote_id
        JOIN job_usage_facts u ON u.job_id=j.job_id JOIN job_settlements settle ON settle.job_id=j.job_id
        WHERE r.run_id=$1 AND j.job_id=$2 AND r.owner_id=$3 AND r.root_budget_id=$4")
        .bind(run).bind(job).bind(c.owner).bind(c.budget).fetch_one(c.pool).await.expect("real cross-service evidence");
    assert_eq!(row.get::<String, _>("status"), "completed");
    assert_eq!(row.get::<String, _>("workspace"), "deleted");
    assert_eq!(row.get::<String, _>("execution_state"), "succeeded");
    assert_eq!(row.get::<String, _>("settlement_state"), "settled");
    let input: GenerateRequest =
        serde_json::from_slice(&std::fs::read(&input_file).expect("test input"))
            .expect("actual request");
    let candidate: orange_protocol::model::ResponseCandidate =
        serde_json::from_slice(&std::fs::read(&output_file).expect("test result"))
            .expect("actual model reply");
    let oracle_root = runtime();
    let oracle = TrustedMeter::launch(oracle_root.path(), profile())
        .await
        .expect("independent actual model counter");
    let input_units = oracle
        .input(input.clone())
        .await
        .expect("actual units")
        .units();
    let output_units = oracle
        .output(input, candidate, 32)
        .await
        .expect("actual result metering")
        .units();
    oracle.shutdown().await.expect("oracle shutdown");
    assert_eq!(row.get::<i64, _>("input_units"), input_units as i64);
    assert_eq!(row.get::<i64, _>("output_units"), output_units as i64);
    let charge = orange_domain::text_price(
        input_units,
        output_units,
        orange_domain::Credits::new(row.get("input_price")).expect("in rate"),
        orange_domain::Credits::new(row.get("output_price")).expect("out rate"),
    )
    .expect("checked charge")
    .units();
    assert_eq!(row.get::<i64, _>("charge"), charge);
    assert!(charge <= row.get::<i64, _>("max_amount"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_settlements WHERE job_id=$1")
        .bind(job)
        .fetch_one(c.pool)
        .await
        .expect("exactly once");
    assert_eq!(count, 1);
    let guest = Command::new(python)
        .arg(repo.join("scripts/cloud-workspace-driver.py"))
        .arg("recover")
        .arg(row.get::<String, _>("allocation_id"))
        .output()
        .await
        .expect("original guest evidence");
    assert!(guest.status.success());
    let receipt: Value = serde_json::from_slice(&guest.stdout).expect("guest receipt");
    assert_eq!(receipt["phase"], "cleaned");
    assert_eq!(
        row.get::<String, _>("evidence_digest"),
        hex::encode(Sha256::digest(
            serde_json::to_vec(&receipt).expect("canonical evidence")
        ))
    );
    let cancelled = browser["cancelled_job_id"].as_str().expect("unstarted Job");
    let stopped: (String, String, i64) = sqlx::query_as(
        "SELECT execution_state,settlement_state,charge FROM jobs WHERE job_id=$1 AND owner_id=$2",
    )
    .bind(cancelled)
    .bind(c.owner)
    .fetch_one(c.pool)
    .await
    .expect("cancelled Job");
    assert_eq!(stopped, ("cancelled".into(), "settled".into(), 0));
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM worker_execution_attempts WHERE job_id=$1 AND start_budget_ms IS NOT NULL")
        .bind(cancelled).fetch_one(c.pool).await.expect("no start grant"),0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM ledger_transactions WHERE owner_id=$1 AND business_key=$2"
        )
        .bind(c.owner)
        .bind(format!("reserve:{job}"))
        .fetch_one(c.pool)
        .await
        .expect("one model reservation"),
        1
    );
    assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM job_inputs i JOIN cloud_model_jobs m USING(job_id) WHERE i.job_id=$1 AND m.run_id=$2)")
        .bind(cancelled).bind(browser["cancelled_run_id"].as_str().expect("Run")).fetch_one(c.pool).await.expect("atomic immutable input binding"));
    let mut result = browser;
    result["settlement_count"] = json!(count);
    result["charge_microcredits"] = json!(charge.to_string());
    result["input_units"] = json!(input_units);
    result["output_units"] = json!(output_units);
    result["cloud_workspace"] = json!("deleted");
    result["platform_host_cleanup"] = receipt;
    result["scope"] = json!(
        "One text-only hosted turn, actual local CPU provider, real model, development credits and fixture identities; no tool dispatch or external payment"
    );
    std::fs::write(
        evidence.join("cloud-model-result.json"),
        serde_json::to_vec_pretty(&result).expect("report"),
    )
    .expect("evidence");
}
