//! Protocol identity fixtures only. Actual process exit and second-job proof
//! are in pricing_cases::cleanup_real, not these declared cleanup reports.
use super::*;
use orange_protocol::cleanup::{CpuCleanupEvidence, cleanup_digest};
pub(super) async fn setup_cleanup(
    cert: &str,
    admit: bool,
) -> (
    InputFixture,
    Running,
    ControlConnection,
    worker::CpuCleanupRequest,
) {
    let digest=request_digest("orange-owned-cpu-v1",&serde_json::json!({"backend_sha256":"d".repeat(64),"model_sha256":"b".repeat(64),"model_bytes":"1000","model_version":"model-test","context_tokens":4096,"startup_timeout_ms":"120000","gpu_layers":0,"parallel":1,"threads":2})).expect("CPU fixture digest");
    let f = setup_model(
        cert,
        &"b".repeat(64),
        &digest,
        "cleanup protocol fixture".into(),
        32,
    )
    .await;
    bind(&f).await;
    if admit {
        sqlx::query("INSERT INTO reviewed_cpu_profiles(execution_profile_id,executable_sha256,model_bytes,context_tokens,startup_timeout_ms) VALUES($1,$2,1000,4096,120000)").bind(&f.profile).bind("d".repeat(64)).execute(f.d.store.pool()).await.expect("explicit CPU admission fixture");
    }
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let root = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("root"));
    let mut n = NodeJournal::open(
        &root.join(format!("node-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("node");
    let mut j = ExecutionJournal::open(
        &root.join(format!("execution-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("journal");
    let mut c =
        ControlConnection::connect_managed_cpu(worker_config(&server, &f.d), &mut n, &mut j, false)
            .await
            .expect("cleanup independent of output writer");
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = c
                .poll_execution("cleanup-fixture", &mut n, &mut j)
                .await
                .expect("poll");
            if let Some(a) = p.assignment {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("assignment");
    let (mut b, s) = broker(&f).await;
    let grant = c
        .reserve_execution(&mut j, &mut b, &s, "cpu", &record.command_id)
        .expect("grant fixture");
    c.authorize_execution(&mut j, &mut n, "cleanup-fixture", &record.command_id)
        .await
        .expect("cloud start")
        .confirm(&mut j, &mut b, &s)
        .expect("confirm");
    let a = assignment(&f).await;
    let ack = worker::Acknowledge {
        command_id: record.command_id,
        payload_digest: a.payload_digest.clone(),
        lease: a.lease.clone(),
        result: worker::AckResult::Accepted as i32,
        execution_journal_id: record.journal_id,
        broker_grant_id: grant.grant_id.clone(),
        broker_epoch: grant.broker_epoch as u64,
        slot_generation: grant.slot_generation as u64,
    };
    let process = serde_json::json!({"instance_id":Uuid::new_v4().to_string(),"model_version":"model-test","executable_sha256":"d".repeat(64),"model_sha256":"b".repeat(64),"phase":"stopped","pid":null,"exit_code":0,"cpu_only":true});
    let proof = request_digest(
        "worker.cpu.cleanup.v1",
        &serde_json::json!({"grant":grant,"process":process,"tasks_joined":true}),
    )
    .expect("declared probe fixture");
    let e:CpuCleanupEvidence=serde_json::from_value(serde_json::json!({"schema_version":1,"grant":grant,"process":process,"local_cleanup_digest":proof})).expect("evidence DTO fixture");
    let digest = cleanup_digest(&ack, &a, &e, "cancelled").expect("request digest");
    let request = worker::CpuCleanupRequest {
        session_id: c.identity().session_id.clone(),
        accepted: Some(ack),
        evidence_json: serde_json::to_vec(&e).expect("JSON"),
        outcome: "cancelled".into(),
        request_digest: digest,
    };
    (f, server, c, request)
}
#[tokio::test]
async fn exact_declared_cpu_cleanup_deduplicates_resource_and_zero_release_facts() {
    let (f, server, c, r) = setup_cleanup("client-cu", true).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let receipt = raw
        .confirm_cpu_cleanup(r.clone())
        .await
        .expect("close")
        .into_inner();
    assert_eq!(
        receipt,
        raw.confirm_cpu_cleanup(r.clone())
            .await
            .expect("same fact")
            .into_inner()
    );
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM worker_cpu_cleanup_receipts WHERE job_id=$1),(SELECT count(*) FROM job_cleanup_releases WHERE job_id=$1)").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("counts");
    assert_eq!(counts, (1, 1));
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM worker_execution_events WHERE attempt_id=$1 AND kind='cpu.cleanup_confirmed'").bind(&receipt.attempt_id).fetch_one(f.d.store.pool()).await.expect("events");
    assert_eq!(events, 1);
    let wallet: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("balance");
    assert_eq!(wallet, (1000, 0));
    let mut changed = r.clone();
    let mut evidence: CpuCleanupEvidence =
        serde_json::from_slice(&changed.evidence_json).expect("fixture");
    evidence.process.instance_id = Uuid::new_v4().to_string();
    evidence.local_cleanup_digest = request_digest(
        "worker.cpu.cleanup.v1",
        &serde_json::json!({"grant":evidence.grant,"process":evidence.process,"tasks_joined":true}),
    )
    .expect("changed fixture digest");
    changed.request_digest = cleanup_digest(
        changed.accepted.as_ref().expect("ack"),
        &assignment(&f).await,
        &evidence,
        &changed.outcome,
    )
    .expect("valid but conflicting claim");
    changed.evidence_json = serde_json::to_vec(&evidence).expect("JSON");
    assert!(
        raw.confirm_cpu_cleanup(changed).await.is_err(),
        "cannot replace the original model-instance fact"
    );
    let mut bad = r;
    bad.request_digest = "a".repeat(64);
    assert!(raw.confirm_cpu_cleanup(bad).await.is_err());
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn unknown_profile_and_non_cpu_claims_do_not_release_cloud_occupancy() {
    let (f, server, c, r) = setup_cleanup("client-cv", false).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    assert!(
        raw.confirm_cpu_cleanup(r.clone()).await.is_err(),
        "no CPU admission"
    );
    let mut e: serde_json::Value = serde_json::from_slice(&r.evidence_json).expect("JSON");
    e["grant"]["device_id"] = serde_json::json!("gpu-0");
    let mut gpu = r;
    gpu.evidence_json = serde_json::to_vec(&e).expect("JSON");
    assert!(raw.confirm_cpu_cleanup(gpu).await.is_err());
    let state: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("occupied");
    assert_eq!(state, "started");
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn new_cleanup_only_session_can_close_old_execution_without_restoring_it() {
    let (f, server, c, mut r) = setup_cleanup("client-cw", true).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let mut h = hello(&f.d.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.cpu_cleanup_version = 1;
    }
    let (sender, mut stream) = control(&mut raw, h).await.expect("cleanup only");
    let w = welcome(&mut stream).await;
    assert!(
        raw.confirm_cpu_cleanup(r.clone()).await.is_err(),
        "old current session denied"
    );
    r.session_id = w.session_id;
    let receipt = raw
        .confirm_cpu_cleanup(r)
        .await
        .expect("original evidence under current node")
        .into_inner();
    assert_eq!(receipt.outcome, "cancelled");
    drop(sender);
    drop(stream);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn resource_receipt_survives_failed_refund_and_replay_releases_once() {
    let (f, server, c, r) = setup_cleanup("client-cy", true).await;
    let job = Uuid::parse_str(&f.job).expect("generated fixture ID");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION cleanup_refund_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM ledger_transactions WHERE transaction_id=NEW.transaction_id AND business_key='release:{job}') THEN RAISE EXCEPTION 'explicit cleanup refund fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER cleanup_refund_fault_{suffix} BEFORE INSERT ON ledger_entries FOR EACH ROW EXECUTE FUNCTION cleanup_refund_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("fault");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let failed = raw.confirm_cpu_cleanup(r.clone()).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER cleanup_refund_fault_{suffix} ON ledger_entries; DROP FUNCTION cleanup_refund_fault_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact fault");
    assert!(failed.is_err());
    let state: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("resource closed");
    assert_eq!(state, "closed");
    assert_eq!(
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("pending finance")
            .settlement_state,
        "pending"
    );
    let wallet: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no partial refund");
    assert_eq!(wallet, (900, 100));
    raw.confirm_cpu_cleanup(r.clone())
        .await
        .expect("repeat original resource receipt, finish refund");
    raw.confirm_cpu_cleanup(r).await.expect("idempotent repeat");
    let transactions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(format!("release:{}", f.job))
            .fetch_one(f.d.store.pool())
            .await
            .expect("once");
    assert_eq!(transactions, 1);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
