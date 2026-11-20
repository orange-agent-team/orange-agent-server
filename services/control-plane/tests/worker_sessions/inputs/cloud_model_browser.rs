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
    let expiry_op = private.path().join("expiry-operation");
    let expiry_locked = private.path().join("expiry-locked");
    let continuing = std::env::var_os("ORANGE_CLOUD_CONTINUATION").is_some();
    let tool_cases = std::env::var_os("ORANGE_CLOUD_DIRECTORY_TOOLS").is_some();
    let web_origin = format!(
        "https://127.0.0.1:{}",
        std::env::var("ORANGE_WEB_COMPUTE_QA_PORT").unwrap_or_else(|_| "43127".into())
    );
    let packet = json!({"api":format!("http://{}/",c.api),"origin":web_origin,"cookie":c.cookie,"user":c.user,"owner":c.owner,"budget":c.budget,"price":c.price,"model":profile().model_version,
        "expiry_operation":expiry_op,"expiry_locked":expiry_locked,"continuation_case":continuing,"continuation_screenshot":evidence.join("actual-tool-continuation.png"),"directory_cases":tool_cases,"tool_screenshot":evidence.join("actual-approved-directory.png"),"gate_quote":gate_quote,"gate_locked":gate_locked,"gate_release":gate_release,"oracle_input":input_file,"oracle_output":output_file,"screenshot":evidence.join("actual-hosted-model.png")});
    let mut fd = browser.stdin.take().expect("private stdin");
    fd.write_all(&serde_json::to_vec(&packet).expect("packet"))
        .await
        .expect("supply fixture");
    drop(fd);
    let (finish, mut finished) = tokio::sync::oneshot::channel();
    let (finish_expiry, expiry_done) = tokio::sync::oneshot::channel();
    let wait_browser = async {
        let result = tokio::time::timeout(
            Duration::from_secs(if continuing {
                540
            } else if tool_cases {
                440
            } else {
                230
            }),
            browser.wait_with_output(),
        )
        .await;
        let _ = finish.send(());
        let _ = finish_expiry.send(());
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
    let expiry = async {
        if !tool_cases {
            return true;
        }
        expire_result_after_dispatch(c.pool, c.owner, &expiry_op, &expiry_locked, expiry_done).await
    };
    let (output, gate_observed, expiry_observed) = tokio::join!(wait_browser, gate, expiry);
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
    if output.status.success() {
        let snapshot: Value = serde_json::from_slice(&output.stdout).expect("browser observation");
        std::fs::write(
            evidence.join("browser-observations.json"),
            serde_json::to_vec_pretty(&snapshot).expect("observation"),
        )
        .expect("partial gate evidence");
    }
    let pid = node.id().expect("owned Host").to_string();
    let _ = Command::new("/bin/kill")
        .args(["-INT", &pid])
        .status()
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(60), node.wait()).await;
    if tool_cases && output.status.success() {
        let observed: Value = serde_json::from_slice(&output.stdout).expect("tool report");
        verify_original_dispatch_receipts(c.pool, &tls, port, &host_id, &observed).await;
    }
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
    assert!(expiry_observed, "uncertain result barrier not reached");
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
    let guest = Command::new(&python)
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
    let mut directory = browser["directory_runs"]
        .as_array()
        .expect("directory cases")
        .clone();
    if tool_cases {
        assert_eq!(directory.len(), if continuing { 4 } else { 3 });
        for case in &mut directory {
            let run = case["run_id"].as_str().expect("tool Run");
            let op = case["operation_id"].as_str().expect("tool op");
            let facts=sqlx::query("SELECT r.status,w.status AS workspace,l.epoch,m.job_id,t.commit_id,i.argument_digest,s.state,d.decision,
                cap.execution_digest,p.permit_id,f.artifact_id,cleanup.allocation_id,cleanup.evidence_digest
                FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_model_jobs m USING(run_id)
                JOIN cloud_tool_responses t ON t.run_id=r.run_id AND t.job_id=m.job_id JOIN cloud_operation_intents i ON i.commit_id=t.commit_id
                JOIN cloud_operation_state s USING(operation_id) JOIN cloud_operation_decisions d USING(operation_id)
                JOIN cloud_directory_capabilities cap ON cap.capability_id=i.capability_id JOIN cloud_directory_revocations v ON v.run_id=r.run_id
                JOIN cloud_run_leases l ON l.run_id=r.run_id LEFT JOIN cloud_dispatch_permits p USING(operation_id)
                LEFT JOIN cloud_operation_results f USING(operation_id) JOIN cloud_workspace_cleanup_receipts cleanup ON cleanup.workspace_id=w.workspace_id
                JOIN job_usage_facts u ON u.job_id=m.job_id AND u.has_tools WHERE r.owner_id=$1 AND r.run_id=$2 AND i.operation_id=$3")
                .bind(c.owner).bind(run).bind(op).fetch_one(c.pool).await.expect("actual committed tool lineage");
            assert_eq!(facts.get::<String, _>("workspace"), "deleted");
            assert_eq!(facts.get::<String, _>("commit_id"), case["commit_id"]);
            assert_eq!(facts.get::<String, _>("decision"), case["decision"]);
            let allocation: String = facts.get("allocation_id");
            let proof = Command::new(&python)
                .arg(repo.join("scripts/cloud-workspace-driver.py"))
                .args(["recover", &allocation])
                .output()
                .await
                .expect("original tool workspace receipt");
            assert!(proof.status.success());
            let guest: Value = serde_json::from_slice(&proof.stdout).expect("kernel evidence");
            assert_eq!(guest["phase"], "cleaned");
            assert_eq!(
                facts.get::<String, _>("evidence_digest"),
                hex::encode(Sha256::digest(serde_json::to_vec(&guest).expect("digest")))
            );
            let permits: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM cloud_dispatch_permits WHERE operation_id=$1",
            )
            .bind(op)
            .fetch_one(c.pool)
            .await
            .expect("permit count");
            if case["decision"] == "approve" {
                assert_eq!(permits, 1);
                assert_eq!(guest["tool_dispatch"]["operation_id"], op);
                assert_eq!(
                    guest["tool_dispatch"]["permit_id"],
                    facts.get::<String, _>("permit_id")
                );
                assert_eq!(
                    guest["tool_dispatch"]["run_epoch"],
                    facts.get::<i64, _>("epoch")
                );
                assert!(
                    guest["tool_observation_sha256"]
                        .as_str()
                        .is_some_and(orange_protocol::valid_digest)
                );
                if case["scenario"] == "expired_after_dispatch" {
                    assert_eq!(facts.get::<String, _>("state"), "unknown");
                    assert!(facts.get::<Option<String>, _>("artifact_id").is_none());
                    assert_eq!(facts.get::<String, _>("status"), "recovery_required");
                } else {
                    assert_eq!(facts.get::<String, _>("state"), "succeeded");
                    assert!(facts.get::<Option<String>, _>("artifact_id").is_some());
                    assert_eq!(
                        facts.get::<String, _>("status"),
                        if case["scenario"] == "continued" {
                            "completed"
                        } else {
                            "paused"
                        }
                    );
                }
            } else {
                assert_eq!(permits, 0);
                assert!(guest.get("tool_dispatch").is_none());
                assert_eq!(facts.get::<String, _>("state"), "failed");
                assert_eq!(facts.get::<String, _>("status"), "paused");
            }
            if case["scenario"] == "continued" {
                let actual=sqlx::query("SELECT t.ordinal,t.invocation_id,t.version_id,t.parent_operation,j.job_id,j.execution_state,j.settlement_state,j.charge,o.kind,
                    js.transaction_id,i.artifact_id FROM cloud_model_turns t JOIN cloud_job_turns m USING(turn_id,run_id) JOIN jobs j USING(job_id)
                    JOIN job_inputs i USING(job_id) JOIN cloud_model_outcomes o USING(turn_id,job_id) JOIN job_settlements js USING(job_id)
                    WHERE t.run_id=$1 AND t.owner_id=$2 AND j.root_budget_id=$3 AND j.owner_id=t.owner_id ORDER BY t.ordinal")
                    .bind(run).bind(c.owner).bind(c.budget).fetch_all(c.pool).await.expect("two true model settlements");
                assert_eq!(actual.len(), 2);
                assert_eq!(actual[0].get::<String, _>("kind"), "tool_request");
                assert_eq!(actual[1].get::<String, _>("kind"), "final");
                assert_eq!(actual[0].get::<String, _>("job_id"), case["job_id"]);
                assert_eq!(
                    actual[1].get::<String, _>("job_id"),
                    case["continuation"]["second_job"]
                );
                assert_ne!(
                    actual[0].get::<String, _>("transaction_id"),
                    actual[1].get::<String, _>("transaction_id")
                );
                assert_eq!(actual[1].get::<String, _>("parent_operation"), op);
                assert_eq!(
                    actual[1].get::<String, _>("version_id"),
                    case["continuation"]["derived_version"]
                );
                let sum: i64 = actual
                    .iter()
                    .map(|r| {
                        assert_eq!(r.get::<String, _>("settlement_state"), "settled");
                        r.get::<i64, _>("charge")
                    })
                    .sum();
                assert_eq!(
                    sum.to_string(),
                    case["continuation"]["settled_microcredits"]
                );
                assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM cloud_workspace_claims c JOIN cloud_workspaces w USING(workspace_id) WHERE w.run_id=$1")
                    .bind(run).fetch_one(c.pool).await.expect("same original workspace"),1);
                case["continuation"]["turn_count"] = json!(2);
                case["continuation"]["two_independent_settlements"] = json!(true);
                case["continuation"]["turn_charges_microcredits"] = json!([
                    actual[0].get::<i64, _>("charge").to_string(),
                    actual[1].get::<i64, _>("charge").to_string()
                ]);
            }
            case["dispatch_count"] = json!(permits);
        }
    }
    let mut result = browser;
    result["directory_runs"] = json!(directory);
    result["settlement_count"] = json!(count);
    result["charge_microcredits"] = json!(charge.to_string());
    result["input_units"] = json!(input_units);
    result["output_units"] = json!(output_units);
    result["cloud_workspace"] = json!("deleted");
    result["platform_host_cleanup"] = receipt;
    result["scope"] = json!(if tool_cases {
        "Actual CPU/gVisor first text turn and three model-requested directory operations: approved, rejected and expired result recording. Original-permit metadata recovery and stale-dispatch refusal verified. No external payment or multi-turn certification."
    } else {
        "One text-only hosted turn, actual local CPU provider, real model, development credits and fixture identities; no tool dispatch or external payment"
    });
    if tool_cases {
        result["original_operation_receipts_verified"] = json!(true);
    }
    std::fs::write(
        evidence.join("cloud-model-result.json"),
        serde_json::to_vec_pretty(&result).expect("report"),
    )
    .expect("evidence");
}

/// In this newly created test database only, delay *recording* a result until
/// the committed permit has expired. The actual syscall and user approval
/// remain unmodified; the service must preserve unknown without replay.
async fn expire_result_after_dispatch(
    pool: &PgPool,
    owner: &str,
    marker: &Path,
    locked: &Path,
    mut finished: tokio::sync::oneshot::Receiver<()>,
) -> bool {
    while !marker.is_file() {
        if !matches!(
            finished.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    let op = std::fs::read_to_string(marker).expect("selected test operation");
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("database identity");
    assert!(db.starts_with("orange_cloud_model_") && db.len() == "orange_cloud_model_".len() + 32);
    let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id)
        JOIN cloud_runs r USING(run_id) WHERE i.operation_id=$1 AND r.owner_id=$2 AND s.state='intended')")
        .bind(&op).bind(owner).fetch_one(pool).await.expect("owned pending intent");
    assert!(pending);
    let mut tx = pool.begin().await.expect("fault gate");
    sqlx::query("SET LOCAL lock_timeout='5s'")
        .execute(&mut *tx)
        .await
        .expect("bounded lock");
    sqlx::query("LOCK TABLE cloud_operation_results IN SHARE MODE")
        .execute(&mut *tx)
        .await
        .expect("hold only test result writes");
    std::fs::write(locked, b"held").expect("ready for decision");
    let expires = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let deadline: Option<i64> = sqlx::query_scalar(
                "SELECT expires_at FROM cloud_dispatch_permits WHERE operation_id=$1",
            )
            .bind(&op)
            .fetch_optional(pool)
            .await
            .expect("observe real dispatch");
            if let Some(deadline) = deadline {
                return Some(deadline);
            }
            if !matches!(
                finished.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .ok()
    .flatten();
    if let Some(at) = expires {
        sqlx::query("SELECT pg_sleep(LEAST(12,GREATEST(0,$1::bigint+1-EXTRACT(EPOCH FROM clock_timestamp()))))")
            .bind(at).execute(&mut *tx).await.expect("wait past actual permit");
    }
    tx.rollback().await.expect("release result writes");
    expires.is_some()
}

async fn verify_original_dispatch_receipts(
    db: &PgPool,
    tls: &Path,
    port: u16,
    host_id: &str,
    observed: &Value,
) {
    use orange_protocol::grpc::host as run;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
    let config = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(
            std::fs::read(tls.join("host-ca.pem")).expect("CA"),
        ))
        .identity(Identity::from_pem(
            std::fs::read(tls.join("host.pem")).expect("leaf"),
            std::fs::read(tls.join("host.key")).expect("key"),
        ));
    let channel = Endpoint::from_shared(format!("https://127.0.0.1:{port}"))
        .expect("dedicated URI")
        .tls_config(config)
        .expect("mTLS")
        .connect()
        .await
        .expect("original Host");
    let mut client = run::run_store_client::RunStoreClient::new(channel);
    for case in observed["directory_runs"].as_array().expect("operations") {
        if case["decision"] != "approve" {
            continue;
        }
        let op = case["operation_id"].as_str().expect("op");
        let id = case["run_id"].as_str().expect("run");
        let row=sqlx::query("SELECT r.owner_id,r.root_budget_id,l.epoch,l.issued_version,l.lease_until,w.workspace_id,p.permit_id,p.allocation_id,f.result_digest
            FROM cloud_run_leases l JOIN cloud_runs r USING(run_id) JOIN cloud_workspaces w ON w.run_id=r.run_id
            JOIN cloud_dispatch_permits p ON p.execution_epoch=l.epoch JOIN cloud_operation_results f ON f.operation_id=p.operation_id
            WHERE l.run_id=$1 AND p.operation_id=$2").bind(id).bind(op).fetch_one(db).await.expect("original durable permit");
        let lookup = run::OperationLookup {
            host_instance_id: host_id.into(),
            run_id: id.into(),
            operation_id: op.into(),
            workspace_id: row.get("workspace_id"),
            permit_id: row.get("permit_id"),
            allocation_id: row.get("allocation_id"),
            run_epoch: row.get::<i64, _>("epoch") as u64,
        };
        let original = client
            .get_operation_receipt(lookup.clone())
            .await
            .expect("original fact without new authority")
            .into_inner();
        assert_eq!(
            original.result_digest,
            row.get::<String, _>("result_digest")
        );
        assert_eq!(
            original.state,
            if case["terminal"] == "unknown" {
                run::operation_record::State::Unknown
            } else {
                run::operation_record::State::Succeeded
            } as i32
        );
        let mut wrong = lookup.clone();
        wrong.permit_id = Uuid::new_v4().to_string();
        assert!(client.get_operation_receipt(wrong).await.is_err());
        let stale = run::RunAuthority {
            run_id: id.into(),
            host_instance_id: host_id.into(),
            run_epoch: lookup.run_epoch,
            run_version: row.get::<i64, _>("issued_version") as u64,
            owner_scope_id: row.get("owner_id"),
            root_budget_id: row.get("root_budget_id"),
            expires_at: Some(prost_types::Timestamp {
                seconds: row.get("lease_until"),
                nanos: 0,
            }),
        };
        assert_eq!(
            client
                .acquire_dispatch(run::DispatchRequest {
                    authority: Some(stale),
                    operation_id: op.into(),
                    expected_version: original.version,
                    argument_digest: original.argument_digest
                })
                .await
                .expect_err("old lease cannot dispatch")
                .code(),
            tonic::Code::FailedPrecondition
        );
    }
}
