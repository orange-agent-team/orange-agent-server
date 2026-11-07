use super::*;
use orange_model_runtime::supervisor::CpuProfile;
use orange_worker::cpu::{CpuExecutionSpec, CpuPhase, CpuResources, cpu_profile_digest, spawn_cpu};
use serde::Deserialize;
#[path = "host_cases.rs"]
mod host_cases;
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
    let (connection, node, journal, record) = receiver_mode(&f, &server, true).await;
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
    assert!(exit.report.cloud_renewals >= 1);
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
    let (connection, node, journal, record) = receiver_mode(&f, &server, true).await;
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
    let (connection, node_journal, journal, record) = receiver_mode(&f, &server, true).await;
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
    let (connection, node_journal, journal, record) = receiver_mode(&f, &server, true).await;
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

#[tokio::test]
async fn actual_owned_model_remains_authorized_beyond_original_lease_then_generates() {
    // The real engine is deliberately held idle while renewal/supervision run;
    // this is a lease-duration test, not a long-generation throughput claim.
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bn",
        &p.model_sha256,
        &digest,
        "Say hello briefly.".into(),
        64,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (mut c, mut n, mut j, record) = receiver_mode(&f, &server, true).await;
    let (mut b, s) = broker(&f).await;
    let grant = c
        .reserve_execution(&mut j, &mut b, &s, "cpu", &record.command_id)
        .expect("reserve");
    let started = c
        .authorize_execution(&mut j, &mut n, "long-cpu", &record.command_id)
        .await
        .expect("cloud start")
        .confirm(&mut j, &mut b, &s)
        .expect("start");
    let request = c
        .fetch_model_input(&mut j, &started, &record.command_id)
        .await
        .expect("actual input");
    let original = assignment(&f)
        .await
        .lease
        .expect("lease")
        .expires_at
        .expect("expires")
        .seconds;
    let root = runtime();
    let (_cancel, receiver) = tokio::sync::watch::channel(false);
    let mut model = orange_model_runtime::supervisor::ManagedCpuModel::launch_from_host(
        root.path(),
        p,
        receiver.clone(),
        Some(Uuid::new_v4().to_string()),
    )
    .await
    .expect("actual owned engine");
    let until = tokio::time::Instant::now() + Duration::from_secs(48);
    let mut next = tokio::time::Instant::now();
    let mut renewals = 0;
    while tokio::time::Instant::now() < until {
        b.renew(&s, &grant, None).expect("local supervision");
        assert!(
            c.heartbeat_node_control("long-cpu", &mut n)
                .await
                .expect("heartbeat")
                .is_empty()
        );
        if tokio::time::Instant::now() >= next {
            let pending = c
                .prepare_renewal(&mut j, &record.command_id)
                .expect("prepare");
            let reply = c.renew_execution(pending).await.expect("live renewal");
            c.confirm_renewal(&mut j, &mut b, &s, reply)
                .expect("Broker renewal");
            renewals += 1;
            next = tokio::time::Instant::now() + Duration::from_secs(10);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
            .fetch_one(f.d.store.pool())
            .await
            .expect("actual clock");
    assert!(
        now > original,
        "must exceed original immutable lease expiry"
    );
    assert!(renewals >= 4);
    b.renew(&s, &grant, None)
        .expect("not expired locally after original lease");
    let result = model
        .client()
        .expect("ready")
        .generate(&request, receiver)
        .await;
    model
        .shutdown()
        .await
        .expect("owned exit even if generation failed");
    assert_eq!(model.record().phase, "stopped");
    let result = result.expect("actual post-original-deadline generation");
    assert!(!result.text.trim().is_empty());
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("engine.lease"))
        .expect("lease");
    lease
        .try_lock()
        .expect("actual exit released inherited lease");
    b.begin_drain(&s, &grant)
        .expect("no substitute cleanup certificate");
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

async fn stopped_by_policy(cert: &str, kind: &str) {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        cert,
        &p.model_sha256,
        &digest,
        "Count from 1 to 10000, write each number without summarizing.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node_journal, journal, record) = receiver_mode(&f, &server, true).await;
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
            capability_snapshot: "policy-cpu".into(),
        },
    );
    generating(&handle).await;
    // Accepted Admin intent is an explicit fixture; delivery, policy application
    // and actual model shutdown are real. MFA is certified in its own tests.
    let (command, action) = enqueue_stop(&f.d, 1, kind).await;
    let mut exit = tokio::time::timeout(Duration::from_secs(25), handle.wait())
        .await
        .expect("bounded policy stop")
        .expect("report");
    assert_eq!(exit.report.outcome, "authority_lost");
    assert!(exit.report.response.is_none());
    assert!(exit.report.cleanup_confirmed);
    assert!(exit.report.node_policy_error.is_none());
    let applied = exit
        .report
        .node_policy
        .as_ref()
        .expect("fresh policy application");
    assert_eq!(applied.command_id, command);
    assert_eq!(applied.policy_generation, "1");
    assert!(
        exit.resources
            .broker
            .node_policy()
            .expect("policy")
            .platform_blocked
    );
    assert!(matches!(
        exit.resources.broker.set_remote_allowed(true),
        Err(BrokerError::PlatformBlocked)
    ));
    assert_eq!(
        exit.resources.broker.slot("cpu").expect("slot").state,
        "idle"
    );
    assert_eq!(
        exit.resources
            .broker
            .applied_node_policy(&command)
            .expect("durable policy")
            .as_ref(),
        Some(applied)
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("process.json")).expect("real exit receipt"),
    )
    .expect("json");
    assert_eq!(state["phase"], "stopped");
    let state: String =
        sqlx::query_scalar("SELECT document->>'state' FROM admin_actions WHERE action_id=$1")
            .bind(action)
            .fetch_one(f.d.store.pool())
            .await
            .expect("Admin state");
    assert_eq!(
        state, "executing",
        "local exit is not a cloud action completion certificate"
    );
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    exit.resources.connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn actual_model_drain_applies_current_policy_and_confirms_owned_exit() {
    stopped_by_policy("client-bq", "node.drain").await;
}
#[tokio::test]
async fn actual_model_quarantine_latches_policy_without_completing_admin_action() {
    stopped_by_policy("client-br", "node.quarantine").await;
}

#[tokio::test]
async fn changed_policy_without_command_stops_model_without_inventing_application() {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let f = setup_model(
        "client-bu",
        &p.model_sha256,
        &digest,
        "Count from 1 to 10000 without summarizing, print every number.".into(),
        1024,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (connection, node_journal, journal, record) = receiver_mode(&f, &server, true).await;
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
            capability_snapshot: "missing-policy-command".into(),
        },
    );
    generating(&handle).await;
    sqlx::query("UPDATE nodes SET policy_generation=1,state='draining' WHERE node_id=$1")
        .bind(&f.d.node)
        .execute(f.d.store.pool())
        .await
        .expect("missing command fault fixture");
    let mut exit = tokio::time::timeout(Duration::from_secs(25), handle.wait())
        .await
        .expect("bounded fail-closed stop")
        .expect("report");
    assert_eq!(exit.report.outcome, "authority_lost");
    assert!(exit.report.response.is_none());
    assert!(exit.report.cleanup_confirmed);
    assert!(exit.report.node_policy.is_none());
    assert_eq!(
        exit.report.node_policy_error.as_deref(),
        Some("POLICY_AUTHORITY_UNCONFIRMED")
    );
    assert_eq!(
        exit.resources
            .broker
            .node_policy()
            .expect("policy")
            .policy_generation,
        "0"
    );
    let request = AcquireRequest {
        request_id: "blocked-new-request".into(),
        device_id: "cpu".into(),
        mode: Holder::Remote,
        profile_id: f.profile.clone(),
        remote: Some(RemoteBinding {
            node_id: f.d.node.clone(),
            attempt_id: "new-attempt".into(),
            lease_id: "new-lease".into(),
            connection_epoch: exit.resources.connection.identity().connection_epoch,
            fencing_token: 99,
        }),
    };
    assert!(matches!(
        exit.resources
            .broker
            .acquire(&exit.resources.supervisor, &request),
        Err(BrokerError::RemoteDisabled)
    ));
    exit.resources.connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
