use super::*;
use orange_worker::{control::ExecutionPreparation, execution_journal::RejectionKind};
use prost::Message;
async fn negative_ack(
    f: &ReceiverFixture,
    command: &str,
    result: worker::AckResult,
) -> worker::Acknowledge {
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT assignment FROM worker_execution_attempts WHERE command_id=$1")
            .bind(command)
            .fetch_one(f.d.store.pool())
            .await
            .expect("original command fixture");
    let a = worker::Assignment::decode(bytes.as_slice()).expect("stored Assignment");
    worker::Acknowledge {
        command_id: a.command_id,
        payload_digest: a.payload_digest,
        lease: a.lease,
        result: result as i32,
        execution_journal_id: String::new(),
        broker_grant_id: String::new(),
        broker_epoch: 0,
        slot_generation: 0,
    }
}
async fn rejected_events(d: &Device, job: &str, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM worker_execution_events e JOIN worker_execution_attempts a USING(attempt_id) WHERE a.job_id=$1 AND e.kind=$2").bind(job).bind(kind).fetch_one(d.store.pool()).await.expect("audit")
}
#[tokio::test]
async fn resource_rejection_is_durable_confirmed_and_preserves_local_holder() {
    let mut f = setup_mode("client-ap", true, true).await;
    let a = receive(&mut f).await;
    let local=match f.broker.acquire(&f.supervisor,&AcquireRequest{request_id:"local-before-offer".into(),device_id:"cpu-fixture".into(),mode:Holder::Local,profile_id:sqlx::query_scalar("SELECT p.execution_profile_id FROM jobs j JOIN quote_execution_plans p USING(quote_id) WHERE j.job_id=$1").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("profile"),remote:None}).expect("local acquire") {AcquireResult::Reserved{grant}=>grant,_=>panic!("local reservation")};
    f.broker
        .confirm_start(&f.supervisor, &local, None)
        .expect("local holder fixture, not actual engine");
    f.broker
        .set_remote_allowed(true)
        .expect("sharing while local owns slot");
    let decision = f
        .connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("known busy");
    let ExecutionPreparation::Rejected(view) = decision else {
        panic!("must reject")
    };
    assert_eq!(view.kind, RejectionKind::Resource);
    assert!(view.receipt_id.is_none());
    assert_eq!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "rejected"
    );
    let receipt = f
        .connection
        .send_rejection(&mut f.journal, &a.command_id)
        .await
        .expect("durable server receipt");
    assert!(receipt.receipt_id.is_some());
    assert_eq!(
        f.connection
            .send_rejection(&mut f.journal, &a.command_id)
            .await
            .expect("same decision"),
        receipt
    );
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").state,
        "running_local"
    );
    assert_eq!(
        f.broker.slot("cpu-fixture").expect("slot").grant.as_ref(),
        Some(&local)
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        1
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "rejection.receipt_created").await,
        1
    );
    assert_eq!(rejected_events(&f.d, &f.job, "start.granted").await, 0);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE entity_id=$1 AND event_type='job.assignment_rejected'",
    )
    .bind(&f.job)
    .fetch_one(f.d.store.pool())
    .await
    .expect("outbox");
    assert_eq!(count, 1);
    let job = f.d.store.job(&f.d.owner, &f.job).await.expect("job");
    assert_eq!(job.execution_state, "queued");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    assert!(
        f.connection
            .authorize_execution(
                &mut f.journal,
                &mut f.node_journal,
                "snapshot",
                &a.command_id
            )
            .await
            .is_err()
    );
    f.connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn protocol_rejection_receipt_does_not_mean_resource_cleanup_or_refund() {
    let mut f = setup_mode("client-aq", false, true).await;
    let a = receive(&mut f).await;
    let decision = f
        .connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("profile refusal");
    let ExecutionPreparation::Rejected(view) = decision else {
        panic!("reject")
    };
    assert_eq!(view.kind, RejectionKind::Protocol);
    let receipt = f
        .connection
        .send_rejection(&mut f.journal, &a.command_id)
        .await
        .expect("receipt");
    assert_eq!(receipt.kind, RejectionKind::Protocol);
    assert_eq!(f.broker.slot("cpu-fixture").expect("slot").state, "idle");
    assert_eq!(
        f.d.store
            .job(&f.d.owner, &f.job)
            .await
            .expect("job")
            .reserved
            .0
            .units(),
        100
    );
    assert!(
        sqlx::query("UPDATE worker_execution_attempts SET rejection_receipt_id=$2 WHERE job_id=$1")
            .bind(&f.job)
            .bind(Uuid::new_v4().to_string())
            .execute(f.d.store.pool())
            .await
            .is_err(),
        "receipt immutable"
    );
    f.connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn lost_local_receipt_commit_recovers_by_historical_query_after_reconnect_and_cancel() {
    let mut f = setup_mode("client-ar", false, true).await;
    let a = receive(&mut f).await;
    f.connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("decision");
    let fault = rusqlite::Connection::open(&f.journal_path).expect("fault");
    fault.execute_batch("CREATE TRIGGER receipt_fault BEFORE UPDATE ON execution_rejections WHEN NEW.receipt IS NOT NULL BEGIN SELECT RAISE(ABORT,'explicit receipt commit fault'); END;").expect("fault fixture");
    assert!(
        f.connection
            .send_rejection(&mut f.journal, &a.command_id)
            .await
            .is_err()
    );
    assert!(!f.connection.is_current_locally());
    assert!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .rejection
            .expect("decision")
            .receipt_id
            .is_none()
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        1
    );
    fault
        .execute_batch("DROP TRIGGER receipt_fault;")
        .expect("repair fixture");
    drop(fault);
    f.connection.close();
    drop(f.journal);
    let mut journal = ExecutionJournal::open(&f.journal_path, f.d.node.clone()).expect("reopen");
    assert_eq!(
        journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .state,
        "rejected"
    );
    let mut connection = ControlConnection::connect_execution_with_rejections(
        worker_config(&f.server, &f.d),
        &mut f.node_journal,
        &mut journal,
    )
    .await
    .expect("new session");
    assert!(
        connection
            .send_rejection(&mut journal, &a.command_id)
            .await
            .is_err(),
        "old ACK cannot be replayed as a new command"
    );
    f.d.store
        .cancel_queued(&f.d.owner, &f.job, "cancel-rejected-job")
        .await
        .expect("explicit queue cancellation");
    let receipt = connection
        .query_rejection(&mut journal, &a.command_id)
        .await
        .expect("historical evidence");
    assert!(receipt.receipt_id.is_some());
    assert_eq!(
        connection
            .query_rejection(&mut journal, &a.command_id)
            .await
            .expect("duplicate history read"),
        receipt
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        1
    );
    assert_eq!(rejected_events(&f.d, &f.job, "start.granted").await, 0);
    let job = f.d.store.job(&f.d.owner, &f.job).await.expect("job");
    assert_eq!(job.execution_state, "cancelled");
    assert_eq!(job.charge.expect("settled zero").0.units(), 0);
    connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn unknown_negative_delivery_is_not_fabricated_by_a_query() {
    let mut f = setup_mode("client-as", false, true).await;
    let a = receive(&mut f).await;
    f.connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("local decision only");
    assert!(
        f.connection
            .query_rejection(&mut f.journal, &a.command_id)
            .await
            .is_err()
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        0
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "rejection.receipt_created").await,
        0
    );
    assert!(
        f.journal
            .record(&a.command_id)
            .expect("read")
            .expect("record")
            .rejection
            .expect("decision")
            .receipt_id
            .is_none()
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
async fn rejection_query_is_scoped_to_authenticated_node_and_original_ack() {
    let mut f = setup_mode("client-at", false, true).await;
    let a = receive(&mut f).await;
    f.connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("decision");
    f.connection
        .send_rejection(&mut f.journal, &a.command_id)
        .await
        .expect("receipt");
    let ack = negative_ack(&f, &a.command_id, worker::AckResult::ProtocolRejected).await;
    let other = device("client-au").await;
    let mut client = f
        .server
        .client(Some(&other.certificate))
        .await
        .expect("other client");
    let mut h = hello(&other.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.rejection_receipts_version = 1;
    }
    let (sender, mut stream) = control(&mut client, h).await.expect("other control");
    welcome(&mut stream).await;
    sender
        .send(worker::WorkerFrame {
            body: Some(worker_frame::Body::RejectionQuery(worker::RejectionQuery {
                rejected: Some(ack),
            })),
        })
        .await
        .expect("query");
    let result = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("bounded");
    assert!(result.is_err());
    assert_eq!(
        rejected_events(&f.d, &f.job, "rejection.receipt_created").await,
        1
    );
    drop(sender);
    drop(stream);
    f.connection.close();
    f.server.stop().await;
}
#[tokio::test]
async fn optional_rejection_capability_does_not_change_legacy_execution_client() {
    let mut f = setup("client-av", false).await;
    let a = receive(&mut f).await;
    assert!(matches!(
        f.connection.prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id
        ),
        Err(orange_worker::control::ControlError::Unsupported)
    ));
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
            .send_rejection(&mut f.journal, &a.command_id)
            .await
            .is_err()
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        0
    );
    f.connection.close();
    f.server.stop().await;
}

#[tokio::test]
async fn stale_supervisor_is_not_misreported_as_an_ordinary_profile_rejection() {
    let mut f = setup_mode("client-aw", false, true).await;
    let a = receive(&mut f).await;
    f.broker
        .invalidate(Invalidation::IdentityChanged)
        .expect("OS identity invalidation fixture");
    assert!(
        f.connection
            .prepare_execution(
                &mut f.journal,
                &mut f.broker,
                &f.supervisor,
                "cpu-fixture",
                &a.command_id
            )
            .is_err()
    );
    let record = f
        .journal
        .record(&a.command_id)
        .expect("read")
        .expect("record");
    assert_eq!(record.state, "received");
    assert!(record.rejection.is_none());
    assert!(
        f.connection
            .send_rejection(&mut f.journal, &a.command_id)
            .await
            .is_err()
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "assignment.rejected").await,
        0
    );
    f.connection.close();
    f.server.stop().await;
}

#[tokio::test]
async fn legacy_rejection_backfill_requires_the_exact_original_ack_digest() {
    let mut f = setup_mode("client-ax", false, true).await;
    let a = receive(&mut f).await;
    f.connection
        .prepare_execution(
            &mut f.journal,
            &mut f.broker,
            &f.supervisor,
            "cpu-fixture",
            &a.command_id,
        )
        .expect("local negative");
    let ack = negative_ack(&f, &a.command_id, worker::AckResult::ProtocolRejected).await;
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT assignment FROM worker_execution_attempts WHERE command_id=$1")
            .bind(&a.command_id)
            .fetch_one(f.d.store.pool())
            .await
            .expect("assignment");
    let original = worker::Assignment::decode(bytes.as_slice()).expect("decode");
    let digest = orange_protocol::execution::acknowledge_digest(&ack, &original)
        .expect("canonical negative");
    // Exact pre-0016 persisted shape: negative ACK accepted, but no receipt
    // columns existed. This does not simulate a new production ACK endpoint.
    sqlx::query(
        "UPDATE worker_execution_attempts SET state='rejected',ack_digest=$2 WHERE command_id=$1",
    )
    .bind(&a.command_id)
    .bind(&digest)
    .execute(f.d.store.pool())
    .await
    .expect("legacy history fixture");
    sqlx::query("INSERT INTO worker_execution_events(event_id,attempt_id,kind) VALUES($1,$2,'assignment.rejected')")
        .bind(Uuid::new_v4().to_string()).bind(original.lease.as_ref().expect("lease").attempt_id.clone()).execute(f.d.store.pool()).await.expect("pre-upgrade negative audit fixture");
    f.connection.close();
    let mut client = f
        .server
        .client(Some(&f.d.certificate))
        .await
        .expect("raw client");
    let mut h = hello(&f.d.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.rejection_receipts_version = 1;
    }
    let (sender, mut stream) = control(&mut client, h).await.expect("current raw session");
    welcome(&mut stream).await;
    let mut wrong = ack;
    wrong.result = worker::AckResult::ResourceRejected as i32;
    sender
        .send(worker::WorkerFrame {
            body: Some(worker_frame::Body::RejectionQuery(worker::RejectionQuery {
                rejected: Some(wrong),
            })),
        })
        .await
        .expect("wrong kind query");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("bounded")
            .is_err()
    );
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT rejection_receipt_id FROM worker_execution_attempts WHERE command_id=$1",
    )
    .bind(&a.command_id)
    .fetch_one(f.d.store.pool())
    .await
    .expect("not backfilled");
    assert!(stored.is_none());
    drop(sender);
    drop(stream);
    let mut connection = ControlConnection::connect_execution_with_rejections(
        worker_config(&f.server, &f.d),
        &mut f.node_journal,
        &mut f.journal,
    )
    .await
    .expect("current receiver");
    let receipt = connection
        .query_rejection(&mut f.journal, &a.command_id)
        .await
        .expect("verified legacy backfill");
    assert_eq!(receipt.kind, RejectionKind::Protocol);
    assert!(receipt.receipt_id.is_some());
    assert_eq!(
        connection
            .query_rejection(&mut f.journal, &a.command_id)
            .await
            .expect("idempotent history"),
        receipt
    );
    assert_eq!(
        rejected_events(&f.d, &f.job, "rejection.receipt_created").await,
        1
    );
    assert_eq!(rejected_events(&f.d, &f.job, "start.granted").await, 0);
    let outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE entity_id=$1 AND event_type='job.assignment_rejected'",
    )
    .bind(&f.job)
    .fetch_one(f.d.store.pool())
    .await
    .expect("historical query has no new Job event");
    assert_eq!(outbox, 0);
    connection.close();
    f.server.stop().await;
}
