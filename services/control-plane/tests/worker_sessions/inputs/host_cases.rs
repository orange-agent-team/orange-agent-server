use super::*;
use std::{os::unix::fs::PermissionsExt, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
type Lines = tokio::io::Lines<BufReader<tokio::process::ChildStdout>>;
fn configuration(
    f: &InputFixture,
    server: &Running,
    p: &CpuProfile,
    root: &std::path::Path,
) -> PathBuf {
    configuration_port(f, server.port, p, root)
}
fn configuration_port(
    f: &InputFixture,
    port: u16,
    p: &CpuProfile,
    root: &std::path::Path,
) -> PathBuf {
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS"));
    let path = root.join("worker.json");
    let config = serde_json::json!({"protocol":1,"environment":"development","state_directory":root,"endpoint":format!("https://127.0.0.1:{port}/"),"server_name":"localhost","server_ca":tls.join("ca.pem"),"certificate":tls.join(format!("{}.pem",f.d.certificate)),"private_key":tls.join(format!("{}.key",f.d.certificate)),"node_id":f.d.node,"allow_remote_contribution":true,"commit_outputs":false,"profile_id":f.profile,"profile":{"executable":p.executable,"executable_sha256":p.executable_sha256,"model":p.model,"model_sha256":p.model_sha256,"model_bytes":p.model_bytes,"model_version":p.model_version,"context_tokens":p.context_tokens,"startup_timeout_seconds":p.startup_timeout.as_secs()}});
    std::fs::write(&path, serde_json::to_vec(&config).expect("config")).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    path
}
fn start(path: &std::path::Path) -> (Child, Lines) {
    let mut child =
        Command::new(std::env::var("ORANGE_WORKER_BINARY").expect("actual built Worker"))
            .arg(path)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("actual process");
    let lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    (child, lines)
}
async fn event(lines: &mut Lines, state: &str) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let line = lines.next_line().await.expect("read").expect("live worker");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("bounded metadata event");
            assert_eq!(value["protocol"], 1);
            assert!(value.get("response").is_none());
            assert!(value.get("text").is_none());
            if value["state"] == state {
                return value;
            }
        }
    })
    .await
    .expect("event deadline")
}
async fn terminate(child: &mut Child) {
    // This is the sole owner of an unreaped direct Child, not PID recovery. If
    // it exits after try_wait(None), the unreaped PID cannot be reused before
    // our wait. No scanned/stored PID or unrelated process is signalled.
    assert!(child.try_wait().expect("owned child status").is_none());
    let pid =
        rustix::process::Pid::from_raw(child.id().expect("live owned child") as i32).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .expect("graceful owned-child signal");
    let status = tokio::time::timeout(Duration::from_secs(25), child.wait())
        .await
        .expect("graceful shutdown bound")
        .expect("wait");
    assert!(
        status.success(),
        "Worker must await cleanup before successful shutdown"
    );
}
async fn ready_model(root: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(bytes) = std::fs::read(root.join("model/process.json"))
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && v["phase"] == "ready"
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("actual backend ready");
}
fn stopped(root: &std::path::Path) {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("model/process.json")).expect("record"))
            .expect("json");
    assert_eq!(value["phase"], "stopped");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("model/engine.lease"))
        .expect("lease");
    file.try_lock().expect("owned engine actually exited");
}
#[tokio::test]
async fn actual_worker_binary_uses_real_os_admission_and_exits_on_owned_signal() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bs",
        &p.model_sha256,
        &digest,
        "Count from 1 to 10000; output every number.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let root = runtime();
    let config = configuration(&f, &server, &p, root.path());
    let (mut child, mut lines) = start(&config);
    event(&mut lines, "ready").await;
    ready_model(root.path()).await;
    terminate(&mut child).await;
    let exit = event(&mut lines, "execution_stopped").await;
    assert_eq!(exit["outcome"], "cancelled");
    assert_eq!(exit["cleanup_confirmed"], true);
    stopped(root.path());
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn actual_worker_binary_keeps_candidate_unsubmitted_and_logs_no_body() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bt",
        &p.model_sha256,
        &digest,
        "Say a short greeting.".into(),
        64,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let root = runtime();
    let config = configuration(&f, &server, &p, root.path());
    let (mut child, mut lines) = start(&config);
    event(&mut lines, "ready").await;
    let exit = event(&mut lines, "execution_stopped").await;
    assert_eq!(exit["outcome"], "computed_unsubmitted");
    assert_eq!(exit["cleanup_confirmed"], true);
    event(&mut lines, "pending_output_commit").await;
    assert!(child.try_wait().expect("still serving control").is_none());
    let (command, _) = enqueue_stop(&f.d, 1, "node.quarantine").await;
    event(&mut lines, "policy_applied").await;
    terminate(&mut child).await;
    stopped(root.path());
    event(&mut lines, "unsubmitted_output_discarded").await;
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("broker.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("inspect actual state");
    let owner: String = db
        .query_row("SELECT owner FROM broker_meta", [], |r| r.get(0))
        .expect("owner");
    assert_eq!(
        owner,
        format!("unix-uid-{}", rustix::process::geteuid().as_raw())
    );
    let applied: i64 = db
        .query_row(
            "SELECT count(*) FROM node_policy_commands WHERE command_id=?",
            [command],
            |r| r.get(0),
        )
        .expect("policy");
    assert_eq!(applied, 1);
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

fn enable_outputs(path: &std::path::Path) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("config")).expect("JSON");
    value["commit_outputs"] = serde_json::json!(true);
    std::fs::write(path, serde_json::to_vec(&value).expect("JSON"))
        .expect("updated private config");
}
#[tokio::test]
async fn actual_worker_binary_delivers_model_output_as_exact_user_artifact() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bz",
        &p.model_sha256,
        &digest,
        "Say a short greeting.".into(),
        64,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let root = runtime();
    let config = configuration(&f, &server, &p, root.path());
    enable_outputs(&config);
    let (mut child, mut lines) = start(&config);
    event(&mut lines, "ready").await;
    event(&mut lines, "response_committed").await;
    let v = f
        .objects
        .job_result(&f.actor, &f.owner, &f.job)
        .await
        .expect("committed reference");
    let (_, bytes, _) = f
        .objects
        .read(&f.actor, &f.owner, &v.upload_id, &v.version_id)
        .await
        .expect("actual customer-authorized bytes");
    let candidate: orange_protocol::model::ResponseCandidate =
        serde_json::from_slice(&bytes.to_vec()).expect("actual model response");
    assert!(!candidate.text.trim().is_empty());
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("execution.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("inspect");
    let digest: String = db
        .query_row("SELECT response_digest FROM cpu_runs", [], |r| r.get(0))
        .expect("model digest");
    assert_eq!(
        digest,
        request_digest("worker.cpu.candidate.v1", &candidate).expect("digest")
    );
    let confirmed: i64 = db
        .query_row(
            "SELECT count(*) FROM execution_outputs WHERE state='confirmed'",
            [],
            |r| r.get(0),
        )
        .expect("confirmed");
    assert_eq!(confirmed, 1);
    drop(db);
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.execution_state, "succeeded");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    terminate(&mut child).await;
    stopped(root.path());
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn committed_output_recovers_after_local_receipt_failure_without_model_replay() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-ca",
        &p.model_sha256,
        &digest,
        "Count from 1 to 10000, print every number.".into(),
        512,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let root = runtime();
    let config = configuration(&f, &server, &p, root.path());
    enable_outputs(&config);
    let (mut child, mut lines) = start(&config);
    event(&mut lines, "ready").await;
    let db = rusqlite::Connection::open(root.path().join("execution.sqlite"))
        .expect("isolated local fault");
    db.execute_batch("CREATE TRIGGER output_receipt_fault BEFORE UPDATE ON execution_outputs WHEN NEW.state='confirmed' BEGIN SELECT RAISE(ABORT,'explicit receipt failure fixture'); END;").expect("fault");
    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("failed receipt exits")
        .expect("wait");
    assert!(!status.success());
    stopped(root.path());
    let before =
        std::fs::read(root.path().join("model/process.json")).expect("original model instance");
    let original = f
        .objects
        .job_result(&f.actor, &f.owner, &f.job)
        .await
        .expect("cloud commit survived");
    db.execute_batch("DROP TRIGGER output_receipt_fault;")
        .expect("repair exact test fault");
    drop(db);
    let (mut recovered, mut events) = start(&config);
    event(&mut events, "response_commit_recovered").await;
    event(&mut events, "ready").await;
    assert_eq!(
        before,
        std::fs::read(root.path().join("model/process.json")).expect("same stopped instance")
    );
    let after = f
        .objects
        .job_result(&f.actor, &f.owner, &f.job)
        .await
        .expect("same output");
    assert_eq!(original, after);
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("execution.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read history");
    let runs: i64 = db
        .query_row("SELECT count(*) FROM cpu_runs", [], |r| r.get(0))
        .expect("runs");
    assert_eq!(runs, 1);
    let confirmed: i64 = db
        .query_row(
            "SELECT count(*) FROM execution_outputs WHERE state='confirmed'",
            [],
            |r| r.get(0),
        )
        .expect("confirmed");
    assert_eq!(confirmed, 1);
    drop(db);
    terminate(&mut recovered).await;
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn actual_user_api_and_output_gateway_share_one_writer_for_real_worker_delivery() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-ce",
        &p.model_sha256,
        &digest,
        "Say a short hello.".into(),
        64,
    )
    .await;
    bind(&f).await;
    let root = runtime();
    let worker_config = configuration_port(&f, 1, &p, root.path());
    enable_outputs(&worker_config);
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS root"));
    let gateway_config = root.path().join("gateway.json");
    std::fs::write(&gateway_config,serde_json::to_vec(&serde_json::json!({"gateway_instance_id":Uuid::new_v4().to_string(),"server_certificate_file":tls.join("server.pem"),"server_key_file":tls.join("server.key"),"client_ca_file":tls.join("ca.pem")})).expect("JSON")).expect("config");
    std::fs::set_permissions(&gateway_config, std::fs::Permissions::from_mode(0o600))
        .expect("private");
    f.objects
        .drain()
        .await
        .expect("release original test writer");
    drop(f.objects);
    let mut api = Command::new(std::env::var("ORANGE_USER_API_BINARY").expect("actual API binary"))
        .env_clear()
        .env("ORANGE_ENV", "development")
        .env(
            "DATABASE_URL",
            std::env::var("ORANGE_TEST_DATABASE_URL").expect("private DB"),
        )
        .env(
            "ORANGE_USER_CONFIG_FILE",
            std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity config"),
        )
        .env("ORANGE_USER_BIND", "127.0.0.1:0")
        .env("ORANGE_ARTIFACT_ROOT", &f.root)
        .env("ORANGE_ARTIFACT_BYTE_LIMIT", "1048576")
        .env("ORANGE_ARTIFACT_RECORD_LIMIT", "100")
        .env("ORANGE_WORKER_GATEWAY_CONFIG", &gateway_config)
        .env("ORANGE_WORKER_GATEWAY_BIND", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("actual API process");
    let mut logs = BufReader::new(api.stderr.take().expect("logs")).lines();
    let (api_address, gateway_address) = tokio::time::timeout(Duration::from_secs(25), async {
        let mut http = None;
        let mut grpc = None;
        loop {
            let line = logs.next_line().await.expect("read").expect("live service");
            if let Some(value) = line.strip_prefix("Development User API listening on ") {
                http = Some(
                    value
                        .split(';')
                        .next()
                        .expect("address")
                        .parse::<std::net::SocketAddr>()
                        .expect("address"),
                );
            }
            if let Some(value) = line.strip_prefix("Development output Gateway listening on ") {
                grpc = Some(value.parse::<std::net::SocketAddr>().expect("address"));
            }
            if let (Some(http), Some(grpc)) = (http, grpc) {
                break (http, grpc);
            }
        }
    })
    .await
    .expect("both real listeners started");
    assert_ne!(api_address, gateway_address);
    assert!(api_address.ip().is_loopback() && gateway_address.ip().is_loopback());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let base = format!("http://{api_address}/api/v1/owners/{}", f.owner);
    assert_eq!(
        client
            .get(format!("{base}/jobs/{}", f.job))
            .headers(f.headers.clone())
            .send()
            .await
            .expect("actual authenticated API")
            .status(),
        200
    );
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&worker_config).expect("config")).expect("JSON");
    config["endpoint"] = serde_json::json!(format!("https://{gateway_address}/"));
    std::fs::write(&worker_config, serde_json::to_vec(&config).expect("JSON"))
        .expect("actual dynamic endpoint");
    let (mut worker, mut events) = start(&worker_config);
    event(&mut events, "ready").await;
    event(&mut events, "response_committed").await;
    let view: orange_protocol::artifacts::ArtifactView = client
        .get(format!("{base}/jobs/{}/result", f.job))
        .headers(f.headers.clone())
        .send()
        .await
        .expect("result")
        .error_for_status()
        .expect("authorized result")
        .json()
        .await
        .expect("view");
    let bytes = client
        .get(format!(
            "{base}/uploads/{}/content?version_id={}",
            view.upload_id, view.version_id
        ))
        .headers(f.headers.clone())
        .send()
        .await
        .expect("content")
        .error_for_status()
        .expect("authorized bytes")
        .bytes()
        .await
        .expect("body");
    let response: orange_protocol::model::ResponseCandidate =
        serde_json::from_slice(&bytes).expect("actual model JSON");
    assert!(!response.text.trim().is_empty());
    terminate(&mut worker).await;
    stopped(root.path());
    assert!(api.try_wait().expect("owned API").is_none());
    let pid =
        rustix::process::Pid::from_raw(api.id().expect("live owned API") as i32).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::INT)
        .expect("owned API graceful shutdown");
    assert!(
        tokio::time::timeout(Duration::from_secs(20), api.wait())
            .await
            .expect("both listeners/I/O stop")
            .expect("wait")
            .success()
    );
    let reopened = ArtifactStore::open_local(f.d.store.pool().clone(), &f.root, 1024 * 1024, 100)
        .await
        .expect("single writer ownership released");
    reopened.drain().await.expect("drain");
}
