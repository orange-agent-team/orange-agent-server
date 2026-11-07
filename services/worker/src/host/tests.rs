use super::*;
use std::os::unix::fs::PermissionsExt;
fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    root
}
#[tokio::test]
async fn real_cpu_admission_refuses_live_lock_unknown_record_and_prior_grant() {
    let root = root();
    let probe = EmptyCpuAdmission {
        root: root.path().to_path_buf(),
    };
    let mut challenge = ProbeChallenge {
        device_id: "cpu".into(),
        nonce: "challenge".into(),
        broker_epoch: 1,
        slot_generation: 1,
        prior_grant: None,
    };
    probe
        .inspect(&challenge)
        .await
        .expect("actual empty private runtime");
    let lease = File::options()
        .read(true)
        .write(true)
        .open(root.path().join("engine.lease"))
        .expect("lease");
    lease.try_lock().expect("held by actual test owner");
    assert!(matches!(
        probe.inspect(&challenge).await,
        Err(BrokerError::Occupied)
    ));
    drop(lease);
    let path = root.path().join("process.json");
    let mut record = orange_model_runtime::supervisor::ProcessRecord {
        instance_id: "prior-instance".into(),
        model_version: "model".into(),
        executable_sha256: "a".repeat(64),
        model_sha256: "b".repeat(64),
        phase: "ready".into(),
        pid: None,
        exit_code: None,
        cpu_only: true,
    };
    std::fs::write(&path, serde_json::to_vec(&record).expect("json")).expect("record");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    assert!(matches!(
        probe.inspect(&challenge).await,
        Err(BrokerError::RecoveryRequired)
    ));
    record.phase = "stopped".into();
    std::fs::write(&path, serde_json::to_vec(&record).expect("json")).expect("record");
    probe
        .inspect(&challenge)
        .await
        .expect("actual free lock and stopped receipt, no unresolved slot grant");
    challenge.prior_grant = Some(Grant {
        grant_id: "old".into(),
        device_id: "cpu".into(),
        broker_epoch: 1,
        slot_generation: 1,
        supervisor_instance: "old-supervisor".into(),
        mode: Holder::Remote,
        profile_id: "profile".into(),
    });
    assert!(
        matches!(
            probe.inspect(&challenge).await,
            Err(BrokerError::RecoveryRequired)
        ),
        "no orphan recovery from a free lock alone"
    );
}
#[test]
fn private_configuration_reader_rejects_aliases_and_public_permissions() {
    let root = root();
    let path = root.path().join("config.json");
    std::fs::write(&path, b"{}").expect("file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("public fixture");
    assert!(config::private_file(&path, 1024).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    assert_eq!(
        config::private_file(&path, 1024).expect("owned private file"),
        b"{}"
    );
    assert!(config::private_file(&path, 1).is_err());
    let alias = root.path().join("alias.json");
    std::os::unix::fs::symlink(&path, &alias).expect("symlink");
    assert!(config::private_file(&alias, 1024).is_err());
    let hard = root.path().join("hard.json");
    std::fs::hard_link(&path, &hard).expect("hardlink");
    assert!(config::private_file(&path, 1024).is_err());
}

#[test]
fn development_entry_requires_explicit_contribution_and_local_endpoint() {
    let root = root();
    let path = root.path().join("worker.json");
    let config = serde_json::json!({"protocol":1,"environment":"development","state_directory":root.path(),"endpoint":"https://127.0.0.1:50051/","server_name":"localhost","server_ca":root.path().join("ca.pem"),"certificate":root.path().join("cert.pem"),"private_key":root.path().join("key.pem"),"node_id":"node","allow_remote_contribution":true,"profile_id":"profile","profile":{"executable":root.path().join("llama-server"),"executable_sha256":"a".repeat(64),"model":root.path().join("model.gguf"),"model_sha256":"b".repeat(64),"model_bytes":10,"model_version":"model","context_tokens":4096,"startup_timeout_seconds":120}});
    let write = |v: &serde_json::Value| {
        std::fs::write(&path, serde_json::to_vec(v).expect("json")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    };
    write(&config);
    WorkerConfig::read(&path)
        .expect("configuration only; missing credential files still prevent run");
    for (key, value) in [
        ("allow_remote_contribution", serde_json::json!(false)),
        ("environment", serde_json::json!("production")),
        (
            "endpoint",
            serde_json::json!("https://example.invalid:443/"),
        ),
        ("protocol", serde_json::json!(2)),
    ] {
        let mut wrong = config.clone();
        wrong[key] = value;
        write(&wrong);
        assert!(WorkerConfig::read(&path).is_err());
    }
    let mut unknown = config;
    unknown["skip_auth"] = serde_json::json!(true);
    write(&unknown);
    assert!(WorkerConfig::read(&path).is_err());
}
