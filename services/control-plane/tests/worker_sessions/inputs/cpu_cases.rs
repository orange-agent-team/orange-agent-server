use super::*;
use orange_model_runtime::supervisor::CpuProfile;
use orange_worker::cpu::{CpuExecutionSpec, CpuPhase, CpuResources, cpu_profile_digest, spawn_cpu};
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RealProfile {
    executable: PathBuf,
    executable_sha256: String,
    model: PathBuf,
    model_sha256: String,
    model_bytes: u64,
}
fn profile() -> CpuProfile {
    let p: RealProfile = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_REAL_WORKER_PROFILE").expect("real fixed model required"),
        )
        .expect("profile file"),
    )
    .expect("profile");
    CpuProfile {
        socket_directory: None,
        executable: p.executable,
        executable_sha256: p.executable_sha256,
        model: p.model,
        model_sha256: p.model_sha256,
        model_bytes: p.model_bytes,
        model_version: "model-test".into(),
        context_tokens: 4096,
        startup_timeout: Duration::from_secs(120),
    }
}
fn runtime() -> tempfile::TempDir {
    let root = tempfile::Builder::new()
        .prefix("oa-cpu-")
        .tempdir_in("/private/tmp")
        .expect("short private root");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    root
}
async fn generating(handle: &orange_worker::cpu::CpuWorkerHandle) {
    let mut phases = handle.phases();
    tokio::time::timeout(Duration::from_secs(30), async {
        while *phases.borrow() != CpuPhase::Generating {
            phases.changed().await.expect("live owner");
        }
    })
    .await
    .expect("actual generation started");
}
#[tokio::test]
async fn actual_worker_model_uses_bound_input_and_confirms_owned_cpu_exit() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("approved profile digest");
    let f = setup_model(
        "client-be",
        &p.model_sha256,
        &digest,
        "Reply with a short greeting.".into(),
        64,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node, journal, record) = receiver(&f, &server).await;
    let (broker, supervisor) = broker(&f).await;
    let root = runtime();
    let resources = CpuResources {
        connection,
        node_journal: node,
        journal,
        broker,
        supervisor,
    };
    let spec = CpuExecutionSpec {
        command_id: record.command_id.clone(),
        profile_id: f.profile.clone(),
        profile: p,
        runtime_directory: root.path().to_path_buf(),
        capability_snapshot: "real-cpu-fixture".into(),
    };
    let mut handle = spawn_cpu(resources, spec);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), handle.wait())
            .await
            .is_err()
    );
    let mut exit = tokio::time::timeout(Duration::from_secs(70), handle.wait())
        .await
        .expect("bounded execution")
        .expect("owner result");
    assert_eq!(exit.report.outcome, "computed_unsubmitted");
    assert!(exit.report.cleanup_confirmed);
    assert!(exit.report.authority_checks >= 1);
    assert!(exit.report.supervision_renewals >= 1);
    let candidate = exit.report.response.as_ref().expect("actual candidate");
    assert!(!candidate.text.trim().is_empty());
    assert!(candidate.tool_calls.is_empty());
    assert_eq!(
        exit.resources.broker.slot("cpu").expect("slot").state,
        "idle"
    );
    assert!(
        exit.resources
            .connection
            .authorize_execution(
                &mut exit.resources.journal,
                &mut exit.resources.node_journal,
                "snapshot",
                &record.command_id
            )
            .await
            .is_err(),
        "cannot repeat an acknowledged model effect"
    );
    let job = f.d.store.job(&f.owner, &f.job).await.expect("cloud job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("process.json")).expect("real stopped receipt"),
    )
    .expect("json");
    assert_eq!(state["phase"], "stopped");
    assert_eq!(state["instance_id"], exit.report.instance_id);
    exit.resources.connection.close();
    server.stop().await;
    f.objects.drain().await.expect("store drain");
}
#[tokio::test]
async fn actual_worker_cancel_keeps_supervision_and_never_returns_a_candidate() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bf",
        &p.model_sha256,
        &digest,
        "Count upwards from 1 to 10000. Do not summarize; output each number.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node, journal, record) = receiver(&f, &server).await;
    let (broker, supervisor) = broker(&f).await;
    let root = runtime();
    let mut handle = spawn_cpu(
        CpuResources {
            connection,
            node_journal: node,
            journal,
            broker,
            supervisor,
        },
        CpuExecutionSpec {
            command_id: record.command_id,
            profile_id: f.profile.clone(),
            profile: p,
            runtime_directory: root.path().to_path_buf(),
            capability_snapshot: "cancel-cpu-fixture".into(),
        },
    );
    let mut phases = handle.phases();
    tokio::time::timeout(Duration::from_secs(30), async {
        while *phases.borrow() != CpuPhase::Generating {
            phases.changed().await.expect("live owner");
        }
    })
    .await
    .expect("real model reached generation");
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.cancel();
    let exit = tokio::time::timeout(Duration::from_secs(20), handle.wait())
        .await
        .expect("cancel cleanup bound")
        .expect("report");
    assert_eq!(exit.report.outcome, "cancelled");
    assert!(exit.report.response.is_none());
    assert!(exit.report.cleanup_confirmed);
    assert_eq!(
        exit.resources.broker.slot("cpu").expect("slot").state,
        "idle"
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("process.json")).expect("stop receipt"),
    )
    .expect("json");
    assert_eq!(state["phase"], "stopped");
    exit.resources.connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn actual_worker_revocation_discards_output_and_joins_the_engine() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bg",
        &p.model_sha256,
        &digest,
        "Count upwards from 1 to 10000. Output every number.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node_journal, journal, record) = receiver(&f, &server).await;
    let (broker, supervisor) = broker(&f).await;
    let root = runtime();
    let mut handle = spawn_cpu(
        CpuResources {
            connection,
            node_journal,
            journal,
            broker,
            supervisor,
        },
        CpuExecutionSpec {
            command_id: record.command_id,
            profile_id: f.profile.clone(),
            profile: p,
            runtime_directory: root.path().to_path_buf(),
            capability_snapshot: "revoke-cpu-fixture".into(),
        },
    );
    generating(&handle).await;
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("actual customer deletion");
    let exit = tokio::time::timeout(Duration::from_secs(20), handle.wait())
        .await
        .expect("bounded revocation")
        .expect("report");
    assert_eq!(exit.report.outcome, "authority_lost");
    assert!(exit.report.response.is_none());
    assert!(exit.report.cleanup_confirmed);
    assert_eq!(
        exit.resources.broker.slot("cpu").expect("slot").state,
        "idle"
    );
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    exit.resources.connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn dropped_handle_still_joins_model_and_persists_cleanup() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bh",
        &p.model_sha256,
        &digest,
        "Count upwards from 1 to 10000. Output every number.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node_journal, journal, record) = receiver(&f, &server).await;
    let (broker, supervisor) = broker(&f).await;
    let root = runtime();
    let mut handle = spawn_cpu(
        CpuResources {
            connection,
            node_journal,
            journal,
            broker,
            supervisor,
        },
        CpuExecutionSpec {
            command_id: record.command_id,
            profile_id: f.profile.clone(),
            profile: p,
            runtime_directory: root.path().to_path_buf(),
            capability_snapshot: "drop-cpu-fixture".into(),
        },
    );
    // Cancelling a wait future must not consume the only completion receiver.
    assert!(
        tokio::time::timeout(Duration::from_millis(1), handle.wait())
            .await
            .is_err()
    );
    generating(&handle).await;
    let mut phases = handle.phases();
    drop(handle);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if *phases.borrow() == CpuPhase::Finished {
                break;
            }
            phases
                .changed()
                .await
                .expect("owner continues after handle drop");
        }
    })
    .await
    .expect("owned cleanup finishes");
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("process.json")).expect("stopped receipt"),
    )
    .expect("json");
    assert_eq!(state["phase"], "stopped");
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("engine.lease"))
        .expect("lease");
    lease
        .try_lock()
        .expect("actual model process released inherited lease");
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
