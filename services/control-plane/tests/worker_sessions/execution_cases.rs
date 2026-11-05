//! Real PostgreSQL and mTLS. Reviewed profile, scheduler plan and Worker ACK are
//! explicit fixtures. No physical model, Broker, device cleanup or billing proof.
use super::*;
use orange_protocol::{CreateBudget, InvocationPurpose, SubmitJob, execution::assignment_digest};
use sqlx::Row;
pub(super) async fn queued(d: &Device) -> String {
    sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,1000)")
        .bind(&d.owner)
        .execute(d.store.pool())
        .await
        .expect("development credits");
    let root = d
        .store
        .create_budget(
            &d.owner,
            &Uuid::new_v4().to_string(),
            &CreateBudget {
                scope_id: Uuid::new_v4().to_string(),
                limit: "1000".to_owned().try_into().expect("money"),
            },
        )
        .await
        .expect("budget");
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'execution-test',100,clock_timestamp()+interval '1 hour')").bind(&quote).bind(&d.owner).bind(&root.root_budget_id).bind(&invocation).bind("a".repeat(64)).execute(d.store.pool()).await.expect("quote fixture");
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,'model-test',$2,$3)").bind(&profile).bind("b".repeat(64)).bind("c".repeat(64)).execute(d.store.pool()).await.expect("reviewed profile fixture");
    sqlx::query("INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline) VALUES($1,$2,$3,$4,date_trunc('second',clock_timestamp()+interval '1 hour'))").bind(&quote).bind(&d.node).bind(&profile).bind("a".repeat(64)).execute(d.store.pool()).await.expect("trusted scheduler fixture");
    d.store
        .submit(
            &d.owner,
            &Uuid::new_v4().to_string(),
            &SubmitJob {
                root_budget_id: root.root_budget_id,
                invocation_id: invocation,
                purpose: InvocationPurpose::Primary,
                quote_id: quote,
                request_projection_digest: "a".repeat(64),
            },
        )
        .await
        .expect("real reservation")
        .job_id
}
fn execution_hello(node: &str, version: u32) -> worker::WorkerFrame {
    let mut frame = hello(node);
    if let Some(worker_frame::Body::Hello(h)) = &mut frame.body {
        h.execution_control_version = version;
    }
    frame
}
async fn assignment(stream: &mut Streaming<worker::ServerFrame>) -> worker::Assignment {
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("assignment deadline")
        .expect("stream")
        .expect("assignment");
    let Some(server_frame::Body::Assignment(a)) = frame.body else {
        panic!("expected Assignment")
    };
    assert_eq!(
        assignment_digest(&a).as_deref(),
        Some(a.payload_digest.as_str())
    );
    a
}
fn ack(a: &worker::Assignment) -> worker::Acknowledge {
    worker::Acknowledge {
        command_id: a.command_id.clone(),
        payload_digest: a.payload_digest.clone(),
        lease: a.lease.clone(),
        result: worker::AckResult::Accepted as i32,
        execution_journal_id: "durable-journal-fixture".into(),
        broker_grant_id: "broker-grant-fixture".into(),
        broker_epoch: 1,
        slot_generation: 7,
    }
}
async fn send(sender: &mpsc::Sender<worker::WorkerFrame>, ack: &worker::Acknowledge) {
    sender
        .send(worker::WorkerFrame {
            body: Some(worker_frame::Body::Acknowledge(ack.clone())),
        })
        .await
        .expect("send ack fixture");
}
async fn start(stream: &mut Streaming<worker::ServerFrame>) -> worker::StartGrant {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream
                .message()
                .await
                .expect("start stream")
                .expect("frame")
                .body
            {
                Some(server_frame::Body::Assignment(_)) => {}
                Some(server_frame::Body::Start(s)) => break s,
                _ => panic!("unexpected frame"),
            }
        }
    })
    .await
    .expect("start deadline")
}
async fn refused(stream: &mut Streaming<worker::ServerFrame>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Err(e) => {
                    assert!(matches!(
                        e.code(),
                        tonic::Code::InvalidArgument
                            | tonic::Code::FailedPrecondition
                            | tonic::Code::Unauthenticated
                    ));
                    break;
                }
                Ok(Some(frame)) => assert!(
                    !matches!(frame.body, Some(server_frame::Body::Start(_))),
                    "must not grant"
                ),
                Ok(None) => panic!("expected explicit refusal"),
            }
        }
    })
    .await
    .expect("refusal deadline");
}
async fn offered(
    certificate: &str,
) -> (
    Device,
    Running,
    String,
    mpsc::Sender<worker::WorkerFrame>,
    Streaming<worker::ServerFrame>,
    worker::Assignment,
) {
    let d = device(certificate).await;
    let job = queued(&d).await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("mTLS");
    let (sender, mut stream) = control(&mut client, execution_hello(&d.node, 1))
        .await
        .expect("control");
    let w = welcome(&mut stream).await;
    assert_eq!(w.execution_control_version, 1);
    let a = assignment(&mut stream).await;
    assert_eq!(a.session_id, w.session_id);
    assert_eq!(a.lease.as_ref().expect("lease").job_id, job);
    (d, server, job, sender, stream, a)
}
async fn grant_count(d: &Device) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM worker_execution_events e JOIN worker_execution_attempts a USING(attempt_id) WHERE a.node_id=$1 AND e.kind='start.granted'").bind(&d.node).fetch_one(d.store.pool()).await.expect("audit")
}
#[tokio::test]
async fn execution_assignment_replay_and_exact_ack_grant_are_durable() {
    let (d, server, job, sender, mut stream, a) = offered("client-w").await;
    assert_eq!(
        d.store
            .job(&d.owner, &job)
            .await
            .expect("job")
            .execution_state,
        "queued"
    );
    assert_eq!(
        assignment(&mut stream).await,
        a,
        "resend same identity and expiry"
    );
    let receipt = ack(&a);
    send(&sender, &receipt).await;
    let grant = start(&mut stream).await;
    assert!(
        (1001..44_000).contains(&grant.start_budget_ms),
        "waiting for the retransmitted offer consumes the original lease budget"
    );
    assert_eq!(grant.lease, receipt.lease);
    assert_eq!(grant.broker_grant_id, receipt.broker_grant_id);
    assert_eq!(grant.slot_generation, 7);
    assert_eq!(grant.execution_journal_id, receipt.execution_journal_id);
    send(&sender, &receipt).await;
    assert_eq!(start(&mut stream).await, grant);
    assert_eq!(grant_count(&d).await, 1);
    let view = d.store.job(&d.owner, &job).await.expect("job");
    assert_eq!(view.execution_state, "running");
    assert_eq!(view.settlement_state, "pending");
    assert!(view.charge.is_none());
    assert!(
        d.store
            .cancel_queued(&d.owner, &job, "after-start")
            .await
            .is_err()
    );
    let mut changed = receipt;
    changed.slot_generation += 1;
    send(&sender, &changed).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 1);
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_legacy_capability_is_opt_out_and_queue_cancel_wins_before_start() {
    let d = device("client-x").await;
    let job = queued(&d).await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("client");
    assert!(
        control(&mut client, execution_hello(&d.node, 3))
            .await
            .is_err()
    );
    let (old, mut stream) = control(&mut client, hello(&d.node)).await.expect("legacy");
    let w = welcome(&mut stream).await;
    assert_eq!(w.execution_control_version, 0);
    heartbeat(&old, &mut stream, &d.node, &w)
        .await
        .expect("legacy unaffected");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_execution_attempts WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("count");
    assert_eq!(count, 0);
    drop(old);
    drop(stream);
    let (sender, mut stream) = control(&mut client, execution_hello(&d.node, 1))
        .await
        .expect("execution");
    welcome(&mut stream).await;
    let a = assignment(&mut stream).await;
    d.store
        .cancel_queued(&d.owner, &job, "cancel-before-ack")
        .await
        .expect("cancel");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    let view = d.store.job(&d.owner, &job).await.expect("job");
    assert_eq!(view.execution_state, "cancelled");
    assert_eq!(view.charge.expect("charge").0.units(), 0);
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_expired_ack_never_starts_or_reassigns_the_uncertain_slot() {
    let (d, server, job, sender, mut stream, a) = offered("client-y").await;
    sqlx::query("UPDATE worker_execution_attempts SET ack_deadline=clock_timestamp()-interval '1 second' WHERE node_id=$1").bind(&d.node).execute(d.store.pool()).await.expect("clock expiry fixture");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    assert_eq!(
        d.store
            .job(&d.owner, &job)
            .await
            .expect("job")
            .settlement_state,
        "pending"
    );
    drop(sender);
    drop(stream);
    let mut client = server.client(Some(&d.certificate)).await.expect("client");
    let (sender, mut stream) = control(&mut client, execution_hello(&d.node, 1))
        .await
        .expect("reconnect");
    let w = welcome(&mut stream).await;
    heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("no reassign on reconnect");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_execution_attempts WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("count");
    assert_eq!(count, 1);
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_node_stop_blocks_an_inflight_ack_and_keeps_reservation() {
    let (d, server, job, sender, mut stream, a) = offered("client-z").await;
    sqlx::query(
        "UPDATE nodes SET state='draining',policy_generation=policy_generation+1 WHERE node_id=$1",
    )
    .bind(&d.node)
    .execute(d.store.pool())
    .await
    .expect("stop policy fixture");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    let view = d.store.job(&d.owner, &job).await.expect("job");
    assert_eq!(view.reserved.0.units(), 100);
    assert!(view.charge.is_none());
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_rejection_is_durable_and_cannot_later_become_accepted() {
    let (d, server, job, sender, mut stream, a) = offered("client-aa").await;
    let mut rejection = ack(&a);
    rejection.result = worker::AckResult::ResourceRejected as i32;
    rejection.execution_journal_id.clear();
    rejection.broker_grant_id.clear();
    rejection.broker_epoch = 0;
    rejection.slot_generation = 0;
    send(&sender, &rejection).await;
    send(&sender, &rejection).await;
    // A later heartbeat is ordered behind both ACKs on this stream.
    let w = worker::Welcome {
        session_id: a.session_id.clone(),
        connection_epoch: a.lease.as_ref().expect("lease").connection_epoch,
        ..Default::default()
    };
    heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("rejection has no StartGrant");
    let row =
        sqlx::query("SELECT state,ack_digest FROM worker_execution_attempts WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("attempt");
    assert_eq!(row.get::<String, _>("state"), "rejected");
    assert!(row.get::<Option<String>, _>("ack_digest").is_some());
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    assert_eq!(
        d.store
            .job(&d.owner, &job)
            .await
            .expect("job")
            .execution_state,
        "queued"
    );
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_reviewed_profile_revocation_blocks_start() {
    let (d, server, _, sender, mut stream, a) = offered("client-ab").await;
    assert!(
        sqlx::query(
            "UPDATE reviewed_execution_profiles SET model_sha256=$2 WHERE execution_profile_id=$1"
        )
        .bind(&a.execution_profile_id)
        .bind("f".repeat(64))
        .execute(d.store.pool())
        .await
        .is_err()
    );
    sqlx::query(
        "UPDATE reviewed_execution_profiles SET enabled=false WHERE execution_profile_id=$1",
    )
    .bind(&a.execution_profile_id)
    .execute(d.store.pool())
    .await
    .expect("withdraw profile");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_ack_cannot_cross_device_or_connection_epoch() {
    let (d, server, _, sender, mut stream, a) = offered("client-ac").await;
    let other = device("client-ad").await;
    let mut c = server
        .client(Some(&other.certificate))
        .await
        .expect("otherclient");
    let (attacker, mut otherstream) = control(&mut c, execution_hello(&other.node, 1))
        .await
        .expect("othercontrol");
    welcome(&mut otherstream).await;
    send(&attacker, &ack(&a)).await;
    refused(&mut otherstream).await;
    assert_eq!(grant_count(&d).await, 0);
    let mut c = server
        .client(Some(&d.certificate))
        .await
        .expect("newclient");
    let (new, mut newstream) = control(&mut c, execution_hello(&d.node, 1))
        .await
        .expect("newcontrol");
    let w = welcome(&mut newstream).await;
    assert!(w.connection_epoch > a.lease.as_ref().expect("lease").connection_epoch);
    send(&new, &ack(&a)).await;
    refused(&mut newstream).await;
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 0);
    drop((attacker, otherstream, new, newstream, sender, stream));
    server.stop().await;
}
#[tokio::test]
async fn execution_start_and_cancel_compete_on_one_job_without_double_release() {
    let (d, server, job, sender, mut stream, a) = offered("client-ae").await;
    let receipt = ack(&a);
    let (_, cancel) = tokio::join!(
        send(&sender, &receipt),
        d.store.cancel_queued(&d.owner, &job, "racing-cancel")
    );
    if cancel.is_ok() {
        refused(&mut stream).await;
        assert_eq!(grant_count(&d).await, 0);
    } else {
        start(&mut stream).await;
        assert_eq!(grant_count(&d).await, 1);
    }
    let row = sqlx::query("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
        .bind(&d.owner)
        .fetch_one(d.store.pool())
        .await
        .expect("wallet");
    assert_eq!(
        row.get::<i64, _>("gift") + row.get::<i64, _>("reserved_gift"),
        1000
    );
    assert_eq!(
        row.get::<i64, _>("reserved_gift"),
        if cancel.is_ok() { 0 } else { 100 }
    );
    drop(sender);
    drop(stream);
    server.stop().await;
}

#[tokio::test]
async fn execution_heartbeat_does_not_renew_and_expired_start_cannot_be_retrieved() {
    let (d, server, job, sender, mut stream, a) = offered("client-af").await;
    send(&sender, &ack(&a)).await;
    start(&mut stream).await;
    let expiry: String = sqlx::query_scalar(
        "SELECT lease_expires_at::text FROM worker_execution_attempts WHERE node_id=$1",
    )
    .bind(&d.node)
    .fetch_one(d.store.pool())
    .await
    .expect("expiry");
    let w = worker::Welcome {
        session_id: a.session_id.clone(),
        connection_epoch: a.lease.as_ref().expect("lease").connection_epoch,
        ..Default::default()
    };
    heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("heartbeat");
    let after: String = sqlx::query_scalar(
        "SELECT lease_expires_at::text FROM worker_execution_attempts WHERE node_id=$1",
    )
    .bind(&d.node)
    .fetch_one(d.store.pool())
    .await
    .expect("expiry");
    assert_eq!(expiry, after);
    sqlx::query("UPDATE worker_execution_attempts SET ack_deadline=clock_timestamp()-interval '2 seconds',lease_expires_at=clock_timestamp()-interval '1 second' WHERE node_id=$1").bind(&d.node).execute(d.store.pool()).await.expect("expired execution fixture");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 1);
    let view = d.store.job(&d.owner, &job).await.expect("job");
    assert_eq!(view.settlement_state, "pending");
    assert_eq!(view.reserved.0.units(), 100);
    assert!(view.charge.is_none());
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn execution_lost_start_reply_and_reconnect_never_replay_or_release() {
    let (d, server, job, sender, stream, a) = offered("client-ag").await;
    send(&sender, &ack(&a)).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while grant_count(&d).await == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("durable grant");
    // Deliberately drop transport without consuming the actual StartGrant.
    drop(sender);
    drop(stream);
    let mut client = server.client(Some(&d.certificate)).await.expect("client");
    let (sender, mut stream) = control(&mut client, execution_hello(&d.node, 1))
        .await
        .expect("reconnect");
    let w = welcome(&mut stream).await;
    heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("no fresh assignment");
    send(&sender, &ack(&a)).await;
    refused(&mut stream).await;
    assert_eq!(grant_count(&d).await, 1);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.entity_id=$1 AND e.event_type='job.start_granted'").bind(&job).fetch_one(d.store.pool()).await.expect("durable event");
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE entity_id=$1 AND event_type='job.start_granted'",
    )
    .bind(&job)
    .fetch_one(d.store.pool())
    .await
    .expect("outbox");
    assert_eq!(count, 1);
    let row =
        sqlx::query("SELECT execution_state,settlement_state,charge FROM jobs WHERE job_id=$1")
            .bind(&job)
            .fetch_one(d.store.pool())
            .await
            .expect("job");
    assert_eq!(row.get::<String, _>("execution_state"), "running");
    assert_eq!(row.get::<String, _>("settlement_state"), "pending");
    assert!(row.get::<Option<i64>, _>("charge").is_none());
    drop(sender);
    drop(stream);
    server.stop().await;
}

#[tokio::test]
async fn execution_revalidates_the_original_submitted_job_digest() {
    let d = device("client-ah").await;
    let job = queued(&d).await;
    // Simulate an inconsistent persisted Job, not a user-facing mutation API.
    // Current Quote/plan still agree, but the original submit digest does not.
    sqlx::query("UPDATE jobs SET purpose='embedding' WHERE job_id=$1")
        .bind(&job)
        .execute(d.store.pool())
        .await
        .expect("inconsistent history fixture");
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("client");
    let (sender, mut stream) = control(&mut client, execution_hello(&d.node, 1))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let next = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("bounded refusal");
    assert!(
        next.is_err(),
        "inconsistent original request must not produce an Assignment"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_execution_attempts WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("attempts");
    assert_eq!(
        count, 0,
        "no durable execution permission from inconsistent history"
    );
    drop(sender);
    drop(stream);
    server.stop().await;
}
