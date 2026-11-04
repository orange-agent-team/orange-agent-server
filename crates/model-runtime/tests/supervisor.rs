#![cfg(unix)]
use orange_model_runtime::{
    GenerateRequest, Message, Role,
    supervisor::{CpuProfile, ManagedCpuModel, ProcessError, ProcessRecord},
};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    binary: PathBuf,
    model: PathBuf,
}
impl Fixture {
    fn new(model: &[u8]) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("oa-")
            .tempdir()
            .expect("temp");
        let base = temp.path().canonicalize().expect("canonical");
        let root = base.join("r");
        fs::create_dir(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private");
        let binary = base.join("backend");
        fs::copy(env!("CARGO_BIN_EXE_model-process-fixture"), &binary).expect("copy fixture");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).expect("executable");
        let path = base.join("model.gguf");
        fs::write(&path, model).expect("fixture model bytes");
        Self {
            _temp: temp,
            root,
            binary,
            model: path,
        }
    }
    fn profile(&self) -> CpuProfile {
        CpuProfile {
            socket_directory: None,
            executable: self.binary.clone(),
            executable_sha256: hex::encode(Sha256::digest(fs::read(&self.binary).expect("binary"))),
            model: self.model.clone(),
            model_sha256: hex::encode(Sha256::digest(fs::read(&self.model).expect("model"))),
            model_bytes: fs::metadata(&self.model).expect("size").len(),
            model_version: "fixture-only".into(),
            context_tokens: 2048,
            startup_timeout: Duration::from_secs(5),
        }
    }
    fn record(&self) -> ProcessRecord {
        serde_json::from_slice(&fs::read(self.root.join("process.json")).expect("record"))
            .expect("json")
    }
    fn host_config(&self) -> PathBuf {
        let profile = self.profile();
        let path = self.root.join("host-profile.json");
        let value = serde_json::json!({"protocol":1,"runtime_directory":self.root,"executable":profile.executable,"executable_sha256":profile.executable_sha256,"model":profile.model,"model_sha256":profile.model_sha256,"model_bytes":profile.model_bytes,"model_version":profile.model_version,"context_tokens":profile.context_tokens,"startup_timeout_seconds":5});
        fs::write(&path, serde_json::to_vec(&value).expect("json")).expect("profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private profile");
        path
    }
}
#[tokio::test]
async fn owned_process_private_ipc_and_explicit_shutdown_release_lease() {
    let f = Fixture::new(b"GGUF-ready");
    let mut managed = ManagedCpuModel::launch(&f.root, f.profile())
        .await
        .expect("fixture process");
    assert_eq!(managed.record().phase, "ready");
    assert!(managed.record().cpu_only);
    assert!(managed.record().pid.is_some());
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::Busy)
    ));
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    let request = GenerateRequest {
        invocation_id: "fixture-invocation".into(),
        model_version: "fixture-only".into(),
        messages: vec![Message::plain(Role::User, "test request".into())],
        tools: vec![],
        max_output_tokens: 32,
    };
    let response = managed
        .client()
        .expect("client")
        .generate(&request, cancel)
        .await
        .expect("real private IPC to explicit fixture");
    assert_eq!(response.text, "test fixture response");
    managed.shutdown().await.expect("reaped");
    assert_eq!(f.record().phase, "stopped");
    assert!(!f.root.join("engine.sock").exists());
    assert!(!f.root.join("api.key").exists());
    assert!(managed.client().is_err());
    let mut next = ManagedCpuModel::launch(&f.root, f.profile())
        .await
        .expect("reuse after receipt");
    assert_ne!(next.record().instance_id, managed.record().instance_id);
    next.shutdown().await.expect("stop");
}

#[tokio::test]
async fn preallocated_host_identity_cannot_be_reused_after_confirmed_stop() {
    let f = Fixture::new(b"GGUF-ready");
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let mut model = ManagedCpuModel::launch_from_host(
        &f.root,
        f.profile(),
        receiver,
        Some("owned-host-instance".into()),
    )
    .await
    .expect("host instance");
    assert_eq!(model.record().instance_id, "owned-host-instance");
    model.shutdown().await.expect("stop");
    let before = fs::read(f.root.join("process.json")).expect("durable receipt");
    assert!(matches!(
        ManagedCpuModel::launch_from_host(
            &f.root,
            f.profile(),
            sender.subscribe(),
            Some("owned-host-instance".into())
        )
        .await,
        Err(ProcessError::Invalid)
    ));
    assert_eq!(
        fs::read(f.root.join("process.json")).expect("no mutation"),
        before
    );
    let mut next = ManagedCpuModel::launch_from_host(
        &f.root,
        f.profile(),
        sender.subscribe(),
        Some("different-host-instance".into()),
    )
    .await
    .expect("fresh identity");
    next.shutdown().await.expect("stop");
}
#[tokio::test]
async fn long_durable_root_uses_short_private_ipc_without_moving_secrets() {
    let mut f = Fixture::new(b"GGUF-ready");
    let ipc = f.root.join("s");
    fs::create_dir(&ipc).expect("IPC directory");
    fs::set_permissions(&ipc, fs::Permissions::from_mode(0o700)).expect("private IPC");
    let long = f
        .root
        .join("durable-application-support-directory-".repeat(3));
    fs::create_dir(&long).expect("long durable directory");
    fs::set_permissions(&long, fs::Permissions::from_mode(0o700)).expect("private state");
    f.root = long;
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::Invalid)
    ));
    let mut profile = f.profile();
    profile.socket_directory = Some(ipc.clone());
    let mut managed = ManagedCpuModel::launch(&f.root, profile)
        .await
        .expect("short private endpoint");
    assert_eq!(f.record().phase, "ready");
    assert!(f.root.join("api.key").is_file());
    assert!(ipc.join("engine.sock").exists());
    assert!(!ipc.join("api.key").exists());
    assert!(!ipc.join("process.json").exists());
    managed
        .client()
        .expect("healthy fixture")
        .health()
        .await
        .expect("actual socket");
    managed.shutdown().await.expect("confirmed shutdown");
    assert!(!ipc.join("engine.sock").exists());
    assert!(!f.root.join("api.key").exists());
    let mut profile = f.profile();
    profile.socket_directory = Some(ipc);
    let mut restarted = ManagedCpuModel::launch(&f.root, profile)
        .await
        .expect("binding permits same state root");
    restarted.shutdown().await.expect("stop restarted fixture");
}

#[tokio::test]
async fn short_ipc_is_bound_to_one_durable_root_and_rejects_public_or_linked_paths() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(b"GGUF-ready");
    let ipc = f.root.join("s");
    fs::create_dir(&ipc).expect("IPC");
    fs::set_permissions(&ipc, fs::Permissions::from_mode(0o755)).expect("public IPC fixture");
    let mut profile = f.profile();
    profile.socket_directory = Some(ipc.clone());
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, profile).await,
        Err(ProcessError::Invalid)
    ));
    assert!(!f.root.join("process.json").exists());
    fs::set_permissions(&ipc, fs::Permissions::from_mode(0o700)).expect("private IPC");
    let alias = f.root.join("linked-ipc");
    symlink(&ipc, &alias).expect("link");
    let mut profile = f.profile();
    profile.socket_directory = Some(alias);
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, profile).await,
        Err(ProcessError::Invalid)
    ));
    let mut profile = f.profile();
    profile.socket_directory = Some(ipc.clone());
    let mut managed = ManagedCpuModel::launch(&f.root, profile)
        .await
        .expect("original owner");
    let binding = fs::read(ipc.join("state-binding")).expect("binding");
    let other = f.root.join("other-state");
    fs::create_dir(&other).expect("other state");
    fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).expect("private other state");
    for stopped in [false, true] {
        if stopped {
            managed.shutdown().await.expect("stop original");
        }
        let mut profile = f.profile();
        profile.socket_directory = Some(ipc.clone());
        assert!(matches!(
            ManagedCpuModel::launch(&other, profile).await,
            Err(ProcessError::RecoveryRequired)
        ));
        assert_eq!(
            fs::read(ipc.join("state-binding")).expect("unchanged binding"),
            binding
        );
        assert!(!other.join("process.json").exists());
        assert!(!other.join("api.key").exists());
    }
}

#[tokio::test]
async fn replaced_socket_is_not_unlinked_or_reported_as_confirmed_cleanup() {
    use std::os::unix::{fs::MetadataExt, net::UnixListener};
    let f = Fixture::new(b"GGUF-ready");
    let mut managed = ManagedCpuModel::launch(&f.root, f.profile())
        .await
        .expect("owned backend");
    let socket = f.root.join("engine.sock");
    // Keep original inode allocated by the live listener while replacing path.
    fs::rename(&socket, f.root.join("old.sock")).expect("move owned socket");
    let unrelated = UnixListener::bind(&socket).expect("unrelated listener fixture");
    let inode = fs::metadata(&socket).expect("replacement metadata").ino();
    assert!(matches!(
        managed.shutdown().await,
        Err(ProcessError::CleanupUnknown)
    ));
    assert_eq!(
        fs::metadata(&socket)
            .expect("unrelated endpoint preserved")
            .ino(),
        inode
    );
    assert_ne!(f.record().phase, "stopped");
    drop(unrelated);
}
#[tokio::test]
async fn corruption_and_nonprivate_paths_never_start_a_process() {
    let f = Fixture::new(b"GGUF-ready");
    let mut profile = f.profile();
    profile.model_sha256 = "0".repeat(64);
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, profile).await,
        Err(ProcessError::Integrity)
    ));
    assert!(!f.root.join("process.json").exists());
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755)).expect("mode");
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::Invalid)
    ));
}
#[tokio::test]
async fn exited_or_unready_backend_has_confirmed_cleanup_before_failure() {
    for bytes in [b"GGUF-exit".as_slice(), b"GGUF-loading".as_slice()] {
        let f = Fixture::new(bytes);
        let mut profile = f.profile();
        profile.startup_timeout = Duration::from_secs(1);
        assert!(matches!(
            ManagedCpuModel::launch(&f.root, profile).await,
            Err(ProcessError::Startup)
        ));
        assert_eq!(f.record().phase, "stopped");
        assert!(!f.root.join("api.key").exists());
    }
}
#[tokio::test]
async fn dropped_owner_records_unknown_and_does_not_claim_reusable_state() {
    let f = Fixture::new(b"GGUF-ready");
    let managed = ManagedCpuModel::launch(&f.root, f.profile())
        .await
        .expect("process");
    drop(managed);
    assert_eq!(f.record().phase, "unknown");
    for _ in 0..100 {
        match ManagedCpuModel::launch(&f.root, f.profile()).await {
            Err(ProcessError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(ProcessError::RecoveryRequired) => return,
            _ => panic!("unknown process must not be automatically replaced"),
        }
    }
    panic!("fixture process did not exit after owned-child kill");
}

#[tokio::test]
async fn parent_crash_cannot_release_the_live_child_execution_lease() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let f = Fixture::new(b"GGUF-ready");
    let mut parent = tokio::process::Command::new(&f.binary)
        .arg("--fixture-parent")
        .arg(&f.root)
        .arg(&f.binary)
        .arg(&f.model)
        .env_clear()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("owned fixture parent");
    let mut lines = BufReader::new(parent.stdout.take().expect("stdout")).lines();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("ready deadline")
            .expect("line")
            .as_deref(),
        Some("FIXTURE_READY")
    );
    parent
        .kill()
        .await
        .expect("kill only the owned fixture parent");
    parent.wait().await.expect("reap parent");
    let live_child_blocks = matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::Busy)
    );
    // Test-only backend endpoint reaps its own process. No PID-based killing of
    // an orphan and no claim that production llama.cpp exposes this endpoint.
    let key = fs::read_to_string(f.root.join("api.key")).expect("test key");
    let client = reqwest::Client::builder()
        .unix_socket(f.root.join("engine.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("private fixture client");
    client
        .get("http://localhost/fixture-stop")
        .bearer_auth(key)
        .send()
        .await
        .expect("stop explicit fixture")
        .error_for_status()
        .expect("stop accepted");
    assert!(
        live_child_blocks,
        "parent death alone must not release the inherited execution lease"
    );
    for _ in 0..100 {
        match ManagedCpuModel::launch(&f.root, f.profile()).await {
            Err(ProcessError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(ProcessError::RecoveryRequired) => return,
            _ => panic!("missing stopped receipt must not be fabricated after parent crash"),
        }
    }
    panic!("test child did not terminate");
}

#[tokio::test]
async fn cancelled_startup_never_persists_ready_or_stopped_without_confirmation() {
    let f = Fixture::new(b"GGUF-loading");
    let root = f.root.clone();
    let profile = f.profile();
    let task = tokio::spawn(async move { ManagedCpuModel::launch(&root, profile).await });
    let mut dispatched = false;
    for _ in 0..100 {
        if let Ok(bytes) = fs::read(f.root.join("process.json"))
            && let Ok(record) = serde_json::from_slice::<ProcessRecord>(&bytes)
            && record.pid.is_some()
        {
            dispatched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    assert!(task.await.is_err());
    assert!(dispatched, "test must interrupt after actual spawn");
    assert_eq!(f.record().phase, "unknown");
    for _ in 0..100 {
        match ManagedCpuModel::launch(&f.root, f.profile()).await {
            Err(ProcessError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(ProcessError::RecoveryRequired) => return,
            _ => panic!("cancelled startup cannot silently restart"),
        }
    }
    panic!("owned fixture did not stop");
}
#[tokio::test]
async fn live_status_observes_real_backend_exit_instead_of_cached_ready() {
    let f = Fixture::new(b"GGUF-ready");
    let mut managed = ManagedCpuModel::launch(&f.root, f.profile())
        .await
        .expect("ready");
    let key = fs::read_to_string(f.root.join("api.key")).expect("test key");
    let client = reqwest::Client::builder()
        .unix_socket(f.root.join("engine.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    client
        .get("http://localhost/fixture-stop")
        .bearer_auth(key)
        .send()
        .await
        .expect("test backend exit");
    for _ in 0..100 {
        if managed.poll_status().expect("owned status").phase == "stopped" {
            assert!(managed.client().is_err());
            assert!(!f.root.join("api.key").exists());
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("stale ready status");
}

#[tokio::test]
async fn supervisor_control_eof_or_invalid_frame_stops_and_reaps_model() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for invalid in [false, true] {
        let f = Fixture::new(b"GGUF-ready");
        let profile = f.host_config();
        let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_orange-model-host"))
            .arg(profile)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("host");
        let mut lines = BufReader::new(host.stdout.take().expect("stdout")).lines();
        let first = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("deadline")
            .expect("read")
            .expect("ready");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&first).expect("json")["record"]["phase"],
            "ready"
        );
        if invalid {
            host.stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"invalid-command\n")
                .await
                .expect("frame");
        } else {
            drop(host.stdin.take());
        }
        let status = tokio::time::timeout(Duration::from_secs(8), host.wait())
            .await
            .expect("host exit")
            .expect("wait");
        assert_eq!(status.success(), !invalid);
        assert_eq!(f.record().phase, "stopped");
        assert!(!f.root.join("api.key").exists());
    }
}

#[tokio::test]
async fn authenticated_private_control_starts_only_after_the_matching_parent_frame() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let f = Fixture::new(b"GGUF-ready");
    let path = f.host_config();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("profile")).expect("json");
    let nonce = "a".repeat(64);
    config["control_nonce"] = serde_json::json!(nonce);
    config["instance_id"] = serde_json::json!("authenticated-host-instance");
    fs::write(&path, serde_json::to_vec(&config).expect("json")).expect("private existing profile");
    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_orange-model-host"))
        .arg(&path)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("host");
    let mut lines = BufReader::new(host.stdout.take().expect("stdout")).lines();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), lines.next_line())
            .await
            .is_err(),
        "no readiness before authentication"
    );
    assert!(!f.root.join("process.json").exists());
    assert!(!f.root.join("engine.lease").exists());
    host.stdin
        .as_mut()
        .expect("input")
        .write_all(format!("auth {nonce}\nping\n").as_bytes())
        .await
        .expect("authenticate");
    let ready = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("deadline")
        .expect("line")
        .expect("ready");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ready).expect("record")["record"]["phase"],
        "ready"
    );
    assert_eq!(f.record().instance_id, "authenticated-host-instance");
    host.stdin
        .as_mut()
        .expect("input")
        .write_all(b"shutdown\n")
        .await
        .expect("shutdown");
    assert!(
        tokio::time::timeout(Duration::from_secs(8), host.wait())
            .await
            .expect("deadline")
            .expect("exit")
            .success()
    );
    assert_eq!(f.record().phase, "stopped");
}

#[tokio::test]
async fn wrong_or_absent_parent_authentication_has_no_model_process_effect() {
    use tokio::io::AsyncWriteExt;
    for wrong in [false, true] {
        let f = Fixture::new(b"GGUF-ready");
        let path = f.host_config();
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("profile")).expect("json");
        config["control_nonce"] = serde_json::json!("a".repeat(64));
        fs::write(&path, serde_json::to_vec(&config).expect("json")).expect("profile");
        let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_orange-model-host"))
            .arg(path)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("host");
        if wrong {
            host.stdin
                .as_mut()
                .expect("input")
                .write_all(format!("auth {}\n", "b".repeat(64)).as_bytes())
                .await
                .expect("wrong auth fixture");
        }
        drop(host.stdin.take());
        assert!(
            !tokio::time::timeout(Duration::from_secs(8), host.wait())
                .await
                .expect("deadline")
                .expect("exit")
                .success()
        );
        assert!(!f.root.join("process.json").exists());
        assert!(!f.root.join("engine.lease").exists());
        assert!(!f.root.join("api.key").exists());
    }
}
#[tokio::test]
async fn ui_process_crash_triggers_owned_supervisor_cleanup() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let f = Fixture::new(b"GGUF-ready");
    let profile = f.host_config();
    let mut ui = tokio::process::Command::new(&f.binary)
        .arg("--fixture-ui")
        .arg(env!("CARGO_BIN_EXE_orange-model-host"))
        .arg(profile)
        .env_clear()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("owned UI fixture");
    let mut lines = BufReader::new(ui.stdout.take().expect("stdout")).lines();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("deadline")
            .expect("line")
            .as_deref(),
        Some("FIXTURE_UI_READY")
    );
    ui.kill().await.expect("kill owned UI fixture");
    ui.wait().await.expect("reap UI");
    for _ in 0..200 {
        if f.record().phase == "stopped" {
            assert!(!f.root.join("api.key").exists());
            let mut next = ManagedCpuModel::launch(&f.root, f.profile())
                .await
                .expect("safe reuse after explicit helper receipt");
            next.shutdown().await.expect("stop");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("parent EOF did not stop supervised model");
}

#[tokio::test]
async fn explicit_startup_cancel_waits_for_actual_child_exit() {
    let f = Fixture::new(b"GGUF-loading");
    let root = f.root.clone();
    let profile = f.profile();
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let task =
        tokio::spawn(
            async move { ManagedCpuModel::launch_with_cancel(&root, profile, receiver).await },
        );
    let mut dispatched = false;
    for _ in 0..100 {
        if let Ok(bytes) = fs::read(f.root.join("process.json"))
            && let Ok(record) = serde_json::from_slice::<ProcessRecord>(&bytes)
            && record.pid.is_some()
        {
            dispatched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    sender.send(true).expect("cancel");
    assert!(matches!(
        task.await.expect("join"),
        Err(ProcessError::Cancelled)
    ));
    assert!(dispatched);
    assert_eq!(f.record().phase, "stopped");
    assert!(!f.root.join("api.key").exists());
}
#[tokio::test]
async fn supervisor_observes_parent_eof_while_model_is_still_loading() {
    let f = Fixture::new(b"GGUF-loading");
    let profile = f.host_config();
    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_orange-model-host"))
        .arg(profile)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("host");
    let mut dispatched = false;
    for _ in 0..100 {
        if let Ok(bytes) = fs::read(f.root.join("process.json"))
            && let Ok(record) = serde_json::from_slice::<ProcessRecord>(&bytes)
            && record.pid.is_some()
        {
            dispatched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(host.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), host.wait())
        .await
        .expect("cleanup deadline")
        .expect("wait");
    assert!(dispatched);
    assert!(status.success());
    assert_eq!(f.record().phase, "stopped");
    assert!(!f.root.join("engine.sock").exists());
}

#[tokio::test]
async fn unexplained_socket_or_linked_record_never_gets_overwritten() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let f = Fixture::new(b"GGUF-ready");
    let listener = UnixListener::bind(f.root.join("engine.sock")).expect("unexplained listener");
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::RecoveryRequired)
    ));
    assert!(f.root.join("engine.sock").exists());
    drop(listener);
    fs::remove_file(f.root.join("engine.sock")).expect("remove fixture");
    symlink(f.root.join("missing-state"), f.root.join("process.json")).expect("dangling record");
    assert!(matches!(
        ManagedCpuModel::launch(&f.root, f.profile()).await,
        Err(ProcessError::Invalid)
    ));
    assert!(
        fs::symlink_metadata(f.root.join("process.json"))
            .expect("preserved link")
            .file_type()
            .is_symlink()
    );
}
