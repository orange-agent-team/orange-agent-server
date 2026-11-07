//! Real mTLS/PG/SQLite/Broker; model/initial device admission are fixtures here.
use super::*;
async fn started(
    f: &InputFixture,
    server: &Running,
    renewal: bool,
) -> (
    ControlConnection,
    NodeJournal,
    ExecutionJournal,
    ExecutionRecord,
    Broker,
    Supervisor,
    Grant,
    worker::ExecutionRenewalRequest,
) {
    let (mut c, mut n, mut j, record) = receiver_mode(f, server, renewal).await;
    let (mut b, s) = broker(f).await;
    let grant = c
        .reserve_execution(&mut j, &mut b, &s, "cpu", &record.command_id)
        .expect("reserve");
    c.authorize_execution(&mut j, &mut n, "renewal-fixture", &record.command_id)
        .await
        .expect("grant")
        .confirm(&mut j, &mut b, &s)
        .expect("confirm");
    let a = assignment(f).await;
    let request = worker::ExecutionRenewalRequest {
        session_id: c.identity().session_id.clone(),
        request_id: Uuid::new_v4().to_string(),
        sequence: 1,
        supervisor_instance_id: grant.supervisor_instance.clone(),
        accepted: Some(worker::Acknowledge {
            command_id: record.command_id.clone(),
            payload_digest: a.payload_digest,
            lease: a.lease,
            result: worker::AckResult::Accepted as i32,
            execution_journal_id: record.journal_id.clone(),
            broker_grant_id: grant.grant_id.clone(),
            broker_epoch: grant.broker_epoch as u64,
            slot_generation: grant.slot_generation as u64,
        }),
    };
    (c, n, j, record, b, s, grant, request)
}
async fn count(f: &InputFixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM worker_execution_renewals r JOIN worker_execution_attempts a USING(attempt_id) WHERE a.job_id=$1").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("rows")
}
#[tokio::test]
async fn renewal_is_durable_exact_and_replay_never_adds_time_or_events() {
    let f = setup("client-bi").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, true).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    let first = raw
        .renew_execution(r.clone())
        .await
        .expect("first")
        .into_inner();
    assert!(first.budget_ms > 1000 && first.budget_ms <= 45000);
    let duplicate = raw
        .renew_execution(r.clone())
        .await
        .expect("duplicate")
        .into_inner();
    assert_eq!(first, duplicate);
    assert_eq!(count(&f).await, 1);
    let mut wrong = r.clone();
    wrong.supervisor_instance_id = "another-supervisor".into();
    assert!(raw.renew_execution(wrong).await.is_err());
    let mut wrong = r.clone();
    wrong.accepted.as_mut().expect("ack").broker_grant_id = "another-grant".into();
    assert!(raw.renew_execution(wrong).await.is_err());
    let mut wrong = r.clone();
    wrong.sequence = 3;
    wrong.request_id = Uuid::new_v4().to_string();
    assert!(raw.renew_execution(wrong).await.is_err());
    let mut second = r.clone();
    second.sequence = 2;
    second.request_id = Uuid::new_v4().to_string();
    assert!(
        raw.renew_execution(second.clone()).await.is_err(),
        "new requests are rate bounded"
    );
    tokio::time::sleep(Duration::from_secs(6)).await;
    let next = raw
        .renew_execution(second)
        .await
        .expect("next")
        .into_inner();
    assert!(
        next.expires_at.expect("expires").seconds >= first.expires_at.expect("expires").seconds
    );
    assert_eq!(
        raw.renew_execution(r)
            .await
            .expect("historical duplicate")
            .into_inner(),
        first
    );
    assert_eq!(count(&f).await, 2);
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM worker_execution_events e JOIN worker_execution_attempts a USING(attempt_id) WHERE a.job_id=$1 AND e.kind='execution.renewed'").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("events");
    assert_eq!(events, 2);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn live_input_deletion_blocks_even_previously_committed_renewal() {
    let f = setup("client-bj").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, true).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    raw.renew_execution(r.clone()).await.expect("first");
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("delete");
    assert!(raw.renew_execution(r).await.is_err());
    assert_eq!(count(&f).await, 1);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn legacy_execution_cannot_call_optional_renewal() {
    let f = setup("client-bk").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, false).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    assert!(raw.renew_execution(r).await.is_err());
    assert_eq!(count(&f).await, 0);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn expired_execution_is_not_revived_by_current_session() {
    let f = setup("client-bl").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, true).await;
    sqlx::query("UPDATE worker_execution_attempts SET ack_deadline=clock_timestamp()-interval '2 seconds',lease_expires_at=clock_timestamp()-interval '1 second' WHERE job_id=$1").bind(&f.job).execute(f.d.store.pool()).await.expect("expired fixture");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    assert!(raw.renew_execution(r).await.is_err());
    assert_eq!(count(&f).await, 0);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn actual_receiver_confirms_once_and_dropped_pending_cannot_reset_anchor() {
    let f = setup("client-bm").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (mut c, _, mut j, record, mut b, s, grant, _) = started(&f, &server, true).await;
    let pending = c
        .prepare_renewal(&mut j, &record.command_id)
        .expect("durable request");
    assert!(c.prepare_renewal(&mut j, &record.command_id).is_err());
    let reply = c
        .renew_execution(pending)
        .await
        .expect("authenticated response");
    c.confirm_renewal(&mut j, &mut b, &s, reply)
        .expect("exact Broker update");
    assert_eq!(count(&f).await, 1);
    assert_eq!(b.slot("cpu").expect("slot").grant.as_ref(), Some(&grant));
    let pending = c
        .prepare_renewal(&mut j, &record.command_id)
        .expect("next local request");
    drop(pending);
    assert!(
        c.prepare_renewal(&mut j, &record.command_id).is_err(),
        "uncertain request cannot get a new monotonic anchor"
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn replacement_session_cannot_inherit_prior_execution_renewal() {
    let f = setup("client-bo").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, true).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    raw.renew_execution(r.clone()).await.expect("first");
    let mut h = hello(&f.d.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.input_artifacts_version = 1;
        h.rejection_receipts_version = 1;
        h.renewal_version = 1;
    }
    let (sender, mut stream) = control(&mut raw, h).await.expect("replacement");
    let w = welcome(&mut stream).await;
    assert!(w.connection_epoch > c.identity().connection_epoch);
    assert!(
        raw.renew_execution(r.clone()).await.is_err(),
        "old authenticated session is not current"
    );
    let mut rebound = r;
    rebound.session_id = w.session_id;
    assert!(
        raw.renew_execution(rebound).await.is_err(),
        "new session cannot inherit the old execution"
    );
    assert_eq!(count(&f).await, 1);
    drop(sender);
    drop(stream);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn database_wait_crossing_prior_expiry_rolls_back_renewal_instead_of_reviving() {
    let f = setup("client-bp").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (c, _, _, _, _, _, _, r) = started(&f, &server, true).await;
    let attempt = &r
        .accepted
        .as_ref()
        .expect("ack")
        .lease
        .as_ref()
        .expect("lease")
        .attempt_id;
    let attempt = Uuid::parse_str(attempt).expect("server-generated UUID");
    let suffix = Uuid::new_v4().simple().to_string();
    // Only validated/generated UUID text enters isolated test-only DDL.
    let ddl = format!(
        "CREATE FUNCTION renewal_delay_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.attempt_id='{attempt}' THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE TRIGGER renewal_delay_{suffix} BEFORE INSERT ON worker_execution_renewals FOR EACH ROW EXECUTE FUNCTION renewal_delay_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("isolated delay trigger");
    sqlx::query("UPDATE worker_execution_attempts SET ack_deadline=clock_timestamp()-interval '1 second',lease_expires_at=clock_timestamp()+interval '2 seconds' WHERE job_id=$1").bind(&f.job).execute(f.d.store.pool()).await.expect("short prior lease fixture");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    let result = raw.renew_execution(r).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER renewal_delay_{suffix} ON worker_execution_renewals; DROP FUNCTION renewal_delay_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact test fault");
    assert!(
        result.is_err(),
        "transaction must not grant a renewed lease after prior authority expired"
    );
    assert_eq!(
        count(&f).await,
        0,
        "decision and extension roll back atomically"
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
