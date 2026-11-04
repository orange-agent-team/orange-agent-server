use super::*;

struct PolicyProbeFixture;
impl orange_worker::broker::ResourceProbe for PolicyProbeFixture {
    fn inspect<'a>(
        &'a self,
        challenge: &'a orange_worker::broker::ProbeChallenge,
    ) -> orange_worker::broker::ProbeFuture<'a> {
        Box::pin(async move {
            Ok(orange_worker::broker::ProbeObservation {
                device_id: challenge.device_id.clone(),
                challenge_nonce: challenge.nonce.clone(),
                prior_grant_id: challenge.prior_grant.as_ref().map(|g| g.grant_id.clone()),
                all_managed_tasks_joined: true,
                all_managed_processes_exited: true,
                device_probe_available: true,
                device_healthy: true,
                evidence_digest: "c".repeat(64),
            })
        })
    }
}
struct PolicyBrokerFixture {
    broker: orange_worker::broker::Broker,
    remote: orange_worker::broker::Supervisor,
    remote_grant: orange_worker::broker::Grant,
    local: orange_worker::broker::Supervisor,
    local_grant: orange_worker::broker::Grant,
}
async fn policy_broker_fixture(d: &Device, epoch: u64) -> PolicyBrokerFixture {
    use orange_worker::broker::*;
    let path =
        PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("private test directory"))
            .join(format!("broker-{}.sqlite", Uuid::new_v4()));
    let configuration = BrokerConfig {
        owner_identity: "policy-test-owner".into(),
        devices: vec!["policy-remote-device".into(), "policy-local-device".into()],
        approved_profiles: vec![ApprovedProfile {
            id: "policy-test-profile".into(),
            model_sha256: "a".repeat(64),
            execution_digest: "b".repeat(64),
        }],
    };
    let mut broker = Broker::open(&path, configuration.clone()).expect("real SQLite Broker");
    broker
        .bind_enrolled_node(&d.node)
        .expect("trusted enrollment fixture");
    for device in &configuration.devices {
        broker
            .reconcile(device, &PolicyProbeFixture)
            .await
            .expect("explicit device probe fixture, not hardware");
    }
    let peer = |name: &str| {
        VerifiedPeer::from_verified_os_identity(
            "policy-test-owner".into(),
            format!("fixture-process-start-{name}"),
            name.into(),
            "n".repeat(32),
        )
        .expect("explicit OS identity fixture")
    };
    let local = broker
        .register_supervisor(peer("local-supervisor"))
        .expect("local");
    let remote = broker
        .register_supervisor(peer("remote-supervisor"))
        .expect("remote");
    let local_request = AcquireRequest {
        request_id: "local-request".into(),
        device_id: "policy-local-device".into(),
        mode: Holder::Local,
        profile_id: "policy-test-profile".into(),
        remote: None,
    };
    let local_grant = match broker
        .acquire(&local, &local_request)
        .expect("local acquire")
    {
        AcquireResult::Reserved { grant } => grant,
        other => panic!("not reserved: {other:?}"),
    };
    broker
        .confirm_start(&local, &local_grant, None)
        .expect("local start state only");
    broker
        .set_remote_allowed(true)
        .expect("local contribution consent");
    let binding = RemoteBinding {
        node_id: d.node.clone(),
        attempt_id: "fixture-attempt".into(),
        lease_id: "fixture-lease".into(),
        connection_epoch: epoch,
        fencing_token: 9,
    };
    let remote_request = AcquireRequest {
        request_id: "remote-request".into(),
        device_id: "policy-remote-device".into(),
        mode: Holder::Remote,
        profile_id: "policy-test-profile".into(),
        remote: Some(binding.clone()),
    };
    let remote_grant = match broker
        .acquire(&remote, &remote_request)
        .expect("remote acquire")
    {
        AcquireResult::Reserved { grant } => grant,
        other => panic!("not reserved: {other:?}"),
    };
    let cloud = VerifiedCloudGrant::from_verified_start(
        binding,
        &remote_grant,
        std::time::Instant::now(),
        Duration::from_secs(45),
    )
    .expect("explicit cloud execution ACK fixture");
    broker
        .confirm_start(&remote, &remote_grant, Some(&cloud))
        .expect("remote start state only, no model process");
    PolicyBrokerFixture {
        broker,
        remote,
        remote_grant,
        local,
        local_grant,
    }
}

#[tokio::test]
async fn current_policy_capability_applies_remote_latch_without_releasing_devices() {
    use orange_worker::{broker::BrokerError, control::ControlConnection};
    let d = device("client-t").await;
    let (command, action) = enqueue_stop(&d, 1, "node.quarantine").await;
    let server = Running::start(&d.store).await;
    let (_, mut journal) = journal(&d);
    let mut client =
        ControlConnection::connect_node_policy(worker_config(&server, &d), &mut journal)
            .await
            .expect("policy v2");
    collect_intent(&mut client, &mut journal).await;
    wait_state(&d, &command, "received").await;
    let mut fixture = policy_broker_fixture(&d, client.identity().connection_epoch).await;
    let authorization = client
        .authorize_node_stop("policy-snapshot", &mut journal, &command)
        .await
        .expect("fresh policy generation and live auth");
    let applied = authorization
        .apply(&mut fixture.broker)
        .expect("atomic policy change");
    assert_eq!(applied.command_id, command);
    assert_eq!(applied.policy_generation, "1");
    assert_eq!(
        fixture
            .broker
            .draining_grants(&fixture.remote)
            .expect("owned remote work"),
        vec![fixture.remote_grant.clone()]
    );
    assert_eq!(
        fixture
            .broker
            .slot(&fixture.remote_grant.device_id)
            .expect("not cleanup")
            .state,
        "draining"
    );
    assert_eq!(
        fixture
            .broker
            .slot(&fixture.local_grant.device_id)
            .expect("local retained")
            .state,
        "running_local"
    );
    fixture
        .broker
        .renew(&fixture.local, &fixture.local_grant, None)
        .expect("local can continue");
    assert!(matches!(
        fixture.broker.set_remote_allowed(true),
        Err(BrokerError::PlatformBlocked)
    ));
    let repeated = client
        .authorize_node_stop("same-policy-snapshot", &mut journal, &command)
        .await
        .expect("recheck")
        .apply(&mut fixture.broker)
        .expect("idempotent policy outcome");
    assert_eq!(repeated, applied);
    let state: String =
        sqlx::query_scalar("SELECT document->>'state' FROM admin_actions WHERE action_id=$1")
            .bind(&action)
            .fetch_one(d.store.pool())
            .await
            .expect("pending cleanup");
    assert_eq!(state, "executing");
    client.close();
    server.stop().await;
}

#[tokio::test]
async fn revoked_device_cannot_apply_a_previously_received_node_policy() {
    use orange_worker::control::{ControlConnection, ControlError};
    let d = device("client-u").await;
    let (command, _) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let (_, mut journal) = journal(&d);
    let mut client =
        ControlConnection::connect_node_policy(worker_config(&server, &d), &mut journal)
            .await
            .expect("policy v2");
    collect_intent(&mut client, &mut journal).await;
    wait_state(&d, &command, "received").await;
    let fixture = policy_broker_fixture(&d, client.identity().connection_epoch).await;
    sqlx::query("UPDATE worker_credentials SET active=false WHERE certificate_sha256=$1")
        .bind(&d.fingerprint)
        .execute(d.store.pool())
        .await
        .expect("revoke after receipt");
    assert!(matches!(
        client
            .authorize_node_stop("fresh-auth-required", &mut journal, &command)
            .await,
        Err(ControlError::Unauthorized)
    ));
    assert!(
        !fixture
            .broker
            .node_policy()
            .expect("no policy effect")
            .platform_blocked
    );
    assert!(
        fixture
            .broker
            .applied_node_policy(&command)
            .expect("no outcome")
            .is_none()
    );
    assert_eq!(
        fixture
            .broker
            .slot(&fixture.remote_grant.device_id)
            .expect("no unauthorized mutation")
            .state,
        "running_remote"
    );
    client.close();
    server.stop().await;
}

#[tokio::test]
async fn latest_cloud_generation_fences_received_but_superseded_policy_before_local_apply() {
    use orange_worker::control::{ControlConnection, ControlError};
    let d = device("client-v").await;
    let (old, _) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let (_, mut journal) = journal(&d);
    let mut client =
        ControlConnection::connect_node_policy(worker_config(&server, &d), &mut journal)
            .await
            .expect("policy v2");
    collect_intent(&mut client, &mut journal).await;
    wait_state(&d, &old, "received").await;
    let mut fixture = policy_broker_fixture(&d, client.identity().connection_epoch).await;
    let (new, _) = enqueue_stop(&d, 2, "node.quarantine").await;
    assert!(matches!(
        client
            .authorize_node_stop("latest-generation", &mut journal, &old)
            .await,
        Err(ControlError::Stale)
    ));
    assert!(
        !fixture
            .broker
            .node_policy()
            .expect("no stale local mutation")
            .platform_blocked
    );
    // Refresh may itself receive the newer intent; it remains available locally.
    if journal.received(&new).expect("local lookup").is_none() {
        collect_intent(&mut client, &mut journal).await;
    }
    wait_state(&d, &new, "received").await;
    let applied = client
        .authorize_node_stop("new-policy", &mut journal, &new)
        .await
        .expect("current generation")
        .apply(&mut fixture.broker)
        .expect("apply only current");
    assert_eq!(applied.policy_generation, "2");
    assert!(
        fixture
            .broker
            .applied_node_policy(&old)
            .expect("old not applied")
            .is_none()
    );
    client.close();
    server.stop().await;
}
