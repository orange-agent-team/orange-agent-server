//! Real E08 client/Gateway/PG and private SQLite/Broker. OS identity and device
//! cleanup probes are fixtures; no model process or GPU certification here.
#[path = "receiver_cases/rejections.rs"]
mod rejection_cases;
use super::*;
use orange_worker::{
    broker::*,
    control::ControlConnection,
    execution_journal::{ExecutionJournal, ExecutionRecord},
    node_journal::NodeJournal,
};
struct ReceiverFixture {
    d: Device,
    server: Running,
    job: String,
    node_journal: NodeJournal,
    journal: ExecutionJournal,
    journal_path: PathBuf,
    broker_path: PathBuf,
    broker: Broker,
    supervisor: Supervisor,
    connection: ControlConnection,
}
async fn setup(certificate: &str, matching_profile: bool) -> ReceiverFixture {
    setup_mode(certificate, matching_profile, false).await
}
async fn setup_mode(
    certificate: &str,
    matching_profile: bool,
    rejection_receipts: bool,
) -> ReceiverFixture {
    let d = device(certificate).await;
    let job = super::execution_cases::queued(&d).await;
    let profile:String=sqlx::query_scalar("SELECT p.execution_profile_id FROM jobs j JOIN quote_execution_plans p USING(quote_id) WHERE j.job_id=$1").bind(&job).fetch_one(d.store.pool()).await.expect("profile fixture");
    let root = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("private directory"));
    let prefix = Uuid::new_v4().to_string();
    let broker_path = root.join(format!("{prefix}-broker.sqlite"));
    let mut broker = Broker::open(
        &broker_path,
        BrokerConfig {
            owner_identity: "worker-test-owner".into(),
            devices: vec!["cpu-fixture".into()],
            approved_profiles: vec![ApprovedProfile {
                id: profile,
                model_sha256: if matching_profile { "b" } else { "f" }.repeat(64),
                execution_digest: "c".repeat(64),
            }],
        },
    )
    .expect("Broker");
    broker
        .bind_enrolled_node(&d.node)
        .expect("enrollment fixture");
    broker
        .reconcile("cpu-fixture", &super::policy_cases::PolicyProbeFixture)
        .await
        .expect("explicit device probe");
    broker.set_remote_allowed(true).expect("local consent");
    let supervisor = broker
        .register_supervisor(
            VerifiedPeer::from_verified_os_identity(
                "worker-test-owner".into(),
                format!("fixture-process-start-{prefix}"),
                format!("supervisor-{prefix}"),
                "n".repeat(32),
            )
            .expect("OS identity fixture"),
        )
        .expect("supervisor");
    let mut node_journal =
        NodeJournal::open(&root.join(format!("{prefix}-node.sqlite")), d.node.clone())
            .expect("node journal");
    let journal_path = root.join(format!("{prefix}-execution.sqlite"));
    let mut journal =
        ExecutionJournal::open(&journal_path, d.node.clone()).expect("execution journal");
    let server = Running::start(&d.store).await;
    let config = worker_config(&server, &d);
    let connection = if rejection_receipts {
        ControlConnection::connect_execution_with_rejections(
            config,
            &mut node_journal,
            &mut journal,
        )
        .await
    } else {
        ControlConnection::connect_execution(config, &mut node_journal, &mut journal).await
    }
    .expect("actual client execution capability2");
    ReceiverFixture {
        d,
        server,
        job,
        node_journal,
        journal,
        journal_path,
        broker_path,
        broker,
        supervisor,
        connection,
    }
}
async fn receive(f: &mut ReceiverFixture) -> ExecutionRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let poll = f
                .connection
                .poll_execution("fixture-capabilities", &mut f.node_journal, &mut f.journal)
                .await
                .expect("poll");
            assert!(poll.node_intents.is_empty());
            if let Some(a) = poll.assignment {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("assignment bound")
}
#[tokio::test]
async fn real_execution_receiver_commits_local_ack_before_exact_broker_start() {
    let mut f = setup("client-ai", true).await;
    let record = receive(&mut f).await;
    assert_eq!(record.state, "received");
    assert_eq!(f.broker.slot("cpu-fixture").expect("slot").state, "idle");
    let grant = f
        .connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &record.command_id,
        )
        .expect("reserve");
    assert_eq!(
        f.journal
            .record(&record.command_id)
            .expect("read")
            .expect("record")
            .state,
        "reserved"
    );
    assert_eq!(
        f.d.store
            .job(&f.d.owner, &f.job)
            .await
            .expect("job")
            .execution_state,
        "queued"
    );
    let authorized = f
        .connection
        .authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &record.command_id,
        )
        .await
        .expect("actual ACK/grant");
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "reserved_remote",
        "ACK/StartGrant reception alone does not ConfirmStart"
    );
    let before = f
        .journal
        .record(&record.command_id)
        .expect("read")
        .expect("record");
    assert_eq!(before.state, "ack_sent");
    assert_eq!(before.grant.as_ref(), Some(&grant));
    let started = authorized
        .confirm(&mut f.journal, &mut f.broker, &f.supervisor)
        .expect("local start barrier");
    assert_eq!(started.grant(), &grant);
    assert!(started.stop_at() > std::time::Instant::now());
    assert!(started.stop_at() <= std::time::Instant::now() + Duration::from_secs(14));
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "running_remote"
    );
    assert_eq!(
        f.journal
            .record(&record.command_id)
            .expect("read")
            .expect("record")
            .state,
        "running"
    );
    assert!(
        f.connection
            .authorize_execution(
                &mut f.journal,
                &mut f.node_journal,
                "fixture-capabilities",
                &record.command_id
            )
            .await
            .is_err(),
        "cannot create another send anchor/start"
    );
    let budget: i64 =
        sqlx::query_scalar("SELECT start_budget_ms FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("stored budget");
    assert!((1001..=45000).contains(&budget));
    f.connection.close();
    drop(f.journal);
    let reopened = ExecutionJournal::open(&f.journal_path, f.d.node.clone()).expect("reopen");
    assert_eq!(
        reopened
            .record(&record.command_id)
            .expect("record")
            .expect("exists")
            .state,
        "unknown"
    );
    f.server.stop().await;
}
#[tokio::test]
async fn real_execution_receiver_rejects_profile_alias_with_different_model_hash() {
    let mut f = setup("client-aj", false).await;
    let a = receive(&mut f).await;
    assert!(
        f.connection
            .reserve_execution(
                &mut f.journal,
                &mut f.broker,
                &f.supervisor,
                "cpu-fixture",
                &a.command_id
            )
            .is_err()
    );
    assert_eq!(f.broker.slot("cpu-fixture").expect("slot").state, "idle");
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "received"
    );
    assert!(
        f.connection
            .authorize_execution(
                &mut f.journal,
                &mut f.node_journal,
                "fixture-capabilities",
                &a.command_id
            )
            .await
            .is_err()
    );
    assert_eq!(
        f.d.store
            .job(&f.d.owner, &f.job)
            .await
            .expect("job")
            .execution_state,
        "queued"
    );
    f.connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn real_execution_local_consent_revocation_wins_before_confirm_start() {
    let mut f = setup("client-ak", true).await;
    let a = receive(&mut f).await;
    f.connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("reserve");
    let authorized = f
        .connection
        .authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &a.command_id,
        )
        .await
        .expect("grant");
    f.broker
        .set_remote_allowed(false)
        .expect("revoke local consent");
    assert!(
        authorized
            .confirm(&mut f.journal, &mut f.broker, &f.supervisor)
            .is_err()
    );
    assert!(!f.connection.is_current_locally());
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "draining"
    );
    assert_ne!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "running"
    );
    assert_eq!(
        f.d.store
            .job(&f.d.owner, &f.job)
            .await
            .expect("job")
            .settlement_state,
        "pending"
    );
    f.connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn real_execution_abandoned_ack_future_cannot_recreate_its_clock_anchor() {
    let mut f = setup("client-al", true).await;
    let a = receive(&mut f).await;
    f.connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("reserve");
    let mut tx = f.d.store.pool().begin().await.expect("blocking fixture");
    sqlx::query("SELECT node_id FROM nodes WHERE node_id=$1 FOR UPDATE")
        .bind(&f.d.node)
        .fetch_one(&mut *tx)
        .await
        .expect("hold ACK authority lock");
    let result = tokio::time::timeout(
        Duration::from_millis(50),
        f.connection.authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &a.command_id,
        ),
    )
    .await;
    assert!(result.is_err(), "real ACK deliberately blocked");
    drop(result);
    tx.rollback().await.expect("release fixture lock");
    assert!(!f.connection.is_current_locally());
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "ack_sent"
    );
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "reserved_remote"
    );
    assert!(
        f.connection
            .authorize_execution(
                &mut f.journal,
                &mut f.node_journal,
                "fixture-capabilities",
                &a.command_id
            )
            .await
            .is_err()
    );
    f.connection.close();
    drop(f.journal);
    let mut reopened = ExecutionJournal::open(&f.journal_path, f.d.node.clone()).expect("reopen");
    assert_eq!(
        reopened
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "unknown"
    );
    let mut new = ControlConnection::connect_execution(
        worker_config(&f.server, &f.d),
        &mut f.node_journal,
        &mut reopened,
    )
    .await
    .expect("new session");
    assert!(
        new.reserve_execution(
            &mut reopened,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id
        )
        .is_err()
    );
    new.close();
    f.server.stop().await;
}
#[tokio::test]
async fn real_execution_unused_start_authorization_expires_without_local_start() {
    let mut f = setup("client-am", true).await;
    let a = receive(&mut f).await;
    f.connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("reserve");
    let authorized = f
        .connection
        .authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &a.command_id,
        )
        .await
        .expect("grant");
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(
        authorized
            .confirm(&mut f.journal, &mut f.broker, &f.supervisor)
            .is_err()
    );
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "reserved_remote"
    );
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "ack_sent"
    );
    f.connection.close();
    f.server.stop().await;
}

#[tokio::test]
async fn real_execution_running_journal_failure_drains_without_returning_engine_permission() {
    let mut f = setup("client-an", true).await;
    let a = receive(&mut f).await;
    f.connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("reserve");
    let authorized = f
        .connection
        .authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &a.command_id,
        )
        .await
        .expect("grant");
    // Explicit database fault in our private fixture, not a production hook.
    let fault = rusqlite::Connection::open(&f.journal_path).expect("fault connection");
    fault.execute_batch("CREATE TRIGGER running_write_fault BEFORE UPDATE ON execution_commands WHEN NEW.state='running' BEGIN SELECT RAISE(ABORT,'explicit disk commit fault'); END;").expect("inject commit failure");
    assert!(
        authorized
            .confirm(&mut f.journal, &mut f.broker, &f.supervisor)
            .is_err()
    );
    assert!(!f.connection.is_current_locally());
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "draining"
    );
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "start_claimed"
    );
    assert_eq!(
        f.d.store
            .job(&f.d.owner, &f.job)
            .await
            .expect("job")
            .settlement_state,
        "pending"
    );
    f.connection.close();
    drop(fault);
    f.server.stop().await;
}

#[tokio::test]
async fn real_execution_confirmation_expiring_during_broker_commit_cannot_escape() {
    let mut f = setup("client-ao", true).await;
    let a = receive(&mut f).await;
    f.connection
        .reserve_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("reserve");
    let authorized = f
        .connection
        .authorize_execution(
            &mut f.journal,
            &mut f.node_journal,
            "fixture-capabilities",
            &a.command_id,
        )
        .await
        .expect("grant");
    let deadline = authorized.confirm_before();
    // Hold a real SQLite write lock across the authority deadline, but release
    // within Broker's100ms busy bound. This is disk delay, not a fake clock.
    let path = f.broker_path.clone();
    let (ready, wait_ready) = tokio::sync::oneshot::channel();
    let blocker = std::thread::spawn(move || {
        let lock = rusqlite::Connection::open(path).expect("fixture lock connection");
        lock.execute_batch("BEGIN IMMEDIATE;")
            .expect("fixture write lock");
        ready.send(()).expect("ready");
        std::thread::sleep(
            (deadline + Duration::from_millis(10))
                .saturating_duration_since(std::time::Instant::now()),
        );
        lock.execute_batch("ROLLBACK;").expect("unlock");
    });
    wait_ready.await.expect("lock ready");
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline - Duration::from_millis(50),
    ))
    .await;
    assert!(
        std::time::Instant::now() < deadline,
        "test must enter confirmation before expiry"
    );
    let result = authorized.confirm(&mut f.journal, &mut f.broker, &f.supervisor);
    blocker.join().expect("fixture thread joined");
    assert!(
        result.is_err(),
        "confirmation must not return engine permission after the durable commit outlives its authority"
    );
    assert!(std::time::Instant::now() >= deadline);
    // Prove this was a committed start followed by conservative drain, not just
    // an unrelated busy-timeout error that happened to reject the operation.
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "running"
    );
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "draining"
    );
    assert!(!f.connection.is_current_locally());
    f.connection.close();
    f.server.stop().await;
}
