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
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS"));
    let path = root.join("worker.json");
    let config = serde_json::json!({"protocol":1,"environment":"development","state_directory":root,"endpoint":format!("https://127.0.0.1:{}/",server.port),"server_name":"localhost","server_ca":tls.join("ca.pem"),"certificate":tls.join(format!("{}.pem",f.d.certificate)),"private_key":tls.join(format!("{}.key",f.d.certificate)),"node_id":f.d.node,"allow_remote_contribution":true,"profile_id":f.profile,"profile":{"executable":p.executable,"executable_sha256":p.executable_sha256,"model":p.model,"model_sha256":p.model_sha256,"model_bytes":p.model_bytes,"model_version":p.model_version,"context_tokens":p.context_tokens,"startup_timeout_seconds":p.startup_timeout.as_secs()}});
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
