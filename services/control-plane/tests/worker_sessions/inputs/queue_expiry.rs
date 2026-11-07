//! Actual deadlines/PG/HTTP inputs; execution ACKs are declared protocol fixtures.
use super::super::execution_cases as execution;
use super::*;
use orange_control_plane::store::StoreError;
async fn due(store: &Store, job: &str) {
    tokio::time::timeout(Duration::from_secs(20),async{loop{let expired:bool=sqlx::query_scalar("SELECT p.job_deadline<=clock_timestamp() FROM jobs j JOIN quote_execution_plans p USING(quote_id) WHERE job_id=$1").bind(job).fetch_one(store.pool()).await.expect("clock");if expired{break;}tokio::time::sleep(Duration::from_millis(25)).await;}}).await.expect("fixture deadline");
}
#[tokio::test]
async fn expired_unbound_job_releases_all_original_buckets_once_even_for_restricted_owner() {
    let d = device("client-ea").await;
    let job = execution::queued_with_balances(&d, 2, [40, 30, 930]).await;
    assert!(!d.store.expire_queued(&job).await.expect("not yet due"));
    sqlx::query("UPDATE owners SET state='restricted' WHERE owner_id=$1")
        .bind(&d.owner)
        .execute(d.store.pool())
        .await
        .expect("restriction does not confiscate holds");
    due(&d.store, &job).await;
    let (a, b) = tokio::join!(d.store.expire_queued(&job), d.store.expire_queued(&job));
    assert_eq!(
        u8::from(a.expect("expiry")) + u8::from(b.expect("duplicate expiry")),
        1
    );
    let row: (String, String, i64) =
        sqlx::query_as("SELECT execution_state,settlement_state,charge FROM jobs WHERE job_id=$1")
            .bind(&job)
            .fetch_one(d.store.pool())
            .await
            .expect("terminal");
    assert_eq!(row, ("failed".into(), "settled".into(), 0));
    let balance:(i64,i64,i64,i64,i64,i64)=sqlx::query_as("SELECT gift,earned,purchased,reserved_gift,reserved_earned,reserved_purchased FROM wallets WHERE owner_id=$1").bind(&d.owner).fetch_one(d.store.pool()).await.expect("original buckets");
    assert_eq!(balance, (40, 30, 930, 0, 0, 0));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(format!("release:{job}"))
            .fetch_one(d.store.pool())
            .await
            .expect("once");
    assert_eq!(count, 1);
    let facts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM job_usage_facts WHERE job_id=$1),(SELECT count(*) FROM worker_cpu_cleanup_receipts WHERE job_id=$1)").bind(&job).fetch_one(d.store.pool()).await.expect("no invented facts");
    assert_eq!(facts, (0, 0));
    assert!(
        sqlx::query("DELETE FROM job_queue_expirations WHERE job_id=$1")
            .bind(&job)
            .execute(d.store.pool())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn expiry_removes_only_its_bound_input_pin_and_preserves_other_references() {
    let f = setup_input_case(
        "client-eb",
        &"b".repeat(64),
        &"c".repeat(64),
        "expiration input fixture".into(),
        32,
        InputAdmission {
            organization: false,
            meter: None,
            deadline_seconds: 3,
        },
    )
    .await;
    bind(&f).await;
    let artifact: String = sqlx::query_scalar("SELECT artifact_id FROM job_inputs WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("input");
    sqlx::query("INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,'independent-expiry-consumer')").bind(&artifact).execute(f.d.store.pool()).await.expect("other reference");
    due(&f.d.store, &f.job).await;
    assert!(f.d.store.expire_queued(&f.job).await.expect("expiry"));
    let pins: Vec<String> =
        sqlx::query_scalar("SELECT reference_id FROM artifact_pins WHERE artifact_id=$1")
            .bind(&artifact)
            .fetch_all(f.d.store.pool())
            .await
            .expect("pins");
    assert_eq!(pins, vec!["independent-expiry-consumer"]);
    f.objects
        .read(&f.actor, &f.owner, &f.upload.upload_id, &f.version)
        .await
        .expect("independent current read unaffected");
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn offered_expiry_cannot_issue_a_late_start_or_claim_resource_cleanup() {
    let d = device("client-ec").await;
    let job = execution::queued_with_deadline(&d, 14).await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, execution::execution_hello(&d.node, 1))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let assignment = execution::assignment(&mut stream).await;
    due(&d.store, &job).await;
    assert!(d.store.expire_queued(&job).await.expect("financial expiry"));
    execution::send(&sender, &execution::ack(&assignment)).await;
    execution::refused(&mut stream).await;
    let state: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&job)
            .fetch_one(d.store.pool())
            .await
            .expect("attempt");
    assert!(
        matches!(state.as_str(), "offered" | "unknown"),
        "expiry does not fabricate resource release"
    );
    let starts:i64=sqlx::query_scalar("SELECT count(*) FROM worker_execution_events e JOIN worker_execution_attempts a USING(attempt_id) WHERE a.job_id=$1 AND e.kind='start.granted'").bind(&job).fetch_one(d.store.pool()).await.expect("no start");
    assert_eq!(starts, 0);
    drop(sender);
    server.stop().await;
}
#[tokio::test]
async fn accepted_execution_never_uses_queue_refund_even_if_a_stale_state_is_injected() {
    let d = device("client-ed").await;
    let job = execution::queued_with_deadline(&d, 14).await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, execution::execution_hello(&d.node, 1))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let assignment = execution::assignment(&mut stream).await;
    execution::send(&sender, &execution::ack(&assignment)).await;
    execution::start(&mut stream).await;
    due(&d.store, &job).await;
    assert!(!d.store.expire_queued(&job).await.expect("running retained"));
    sqlx::query("UPDATE jobs SET execution_state='queued' WHERE job_id=$1")
        .bind(&job)
        .execute(d.store.pool())
        .await
        .expect("explicit integrity-fault fixture");
    let failed = d.store.expire_queued(&job).await;
    sqlx::query("UPDATE jobs SET execution_state='running' WHERE job_id=$1")
        .bind(&job)
        .execute(d.store.pool())
        .await
        .expect("restore only faulted fixture");
    assert!(matches!(failed, Err(StoreError::Conflict)));
    let held: i64 = sqlx::query_scalar("SELECT reserved_gift FROM wallets WHERE owner_id=$1")
        .bind(&d.owner)
        .fetch_one(d.store.pool())
        .await
        .expect("no refund");
    assert_eq!(held, 100);
    drop(sender);
    server.stop().await;
}

#[tokio::test]
async fn expiry_ledger_failure_rolls_back_and_concurrent_user_cancel_refunds_once() {
    let d = device("client-ee").await;
    let job = execution::queued_with_deadline(&d, 2).await;
    due(&d.store, &job).await;
    let id = Uuid::parse_str(&job).expect("fixture ID");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION expiry_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM ledger_transactions WHERE transaction_id=NEW.transaction_id AND business_key='release:{id}') THEN RAISE EXCEPTION 'explicit expiry ledger fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER expiry_fault_{suffix} BEFORE INSERT ON ledger_entries FOR EACH ROW EXECUTE FUNCTION expiry_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(d.store.pool())
        .await
        .expect("scoped fault");
    let failed = d.store.expire_queued(&job).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER expiry_fault_{suffix} ON ledger_entries; DROP FUNCTION expiry_fault_{suffix}();"))).execute(d.store.pool()).await.expect("remove exact fault");
    assert!(failed.is_err());
    let pending = d.store.job(&d.owner, &job).await.expect("rollback");
    assert_eq!(pending.execution_state, "queued");
    assert_eq!(pending.settlement_state, "pending");
    let hold: i64 = sqlx::query_scalar("SELECT reserved_gift FROM wallets WHERE owner_id=$1")
        .bind(&d.owner)
        .fetch_one(d.store.pool())
        .await
        .expect("hold");
    assert_eq!(hold, 100);
    let (expiry, cancel) = tokio::join!(
        d.store.expire_queued(&job),
        d.store.cancel_queued(&d.owner, &job, "concurrent-cancel")
    );
    expiry.expect("expiry retry");
    assert!(cancel.is_ok() || matches!(cancel, Err(StoreError::InvalidTransition)));
    let final_job = d.store.job(&d.owner, &job).await.expect("terminal");
    assert!(matches!(
        final_job.execution_state.as_str(),
        "failed" | "cancelled"
    ));
    assert_eq!(final_job.settlement_state, "settled");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(format!("release:{job}"))
            .fetch_one(d.store.pool())
            .await
            .expect("once");
    assert_eq!(count, 1);
}
#[tokio::test]
async fn actual_user_api_expires_queued_work_without_an_artifact_writer() {
    use std::process::Stdio;
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
    };
    let d = device("client-ef").await;
    let job = execution::queued_with_deadline(&d, 2).await;
    due(&d.store, &job).await;
    let mut child = Command::new(std::env::var("ORANGE_USER_API_BINARY").expect("actual API"))
        .env_clear()
        .env("ORANGE_ENV", "development")
        .env(
            "DATABASE_URL",
            std::env::var("ORANGE_TEST_DATABASE_URL").expect("DB"),
        )
        .env(
            "ORANGE_USER_CONFIG_FILE",
            std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("config"),
        )
        .env("ORANGE_USER_BIND", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("owned API");
    let mut lines = BufReader::new(child.stderr.take().expect("diagnostics")).lines();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let line = lines.next_line().await.expect("read").expect("live API");
            if line.starts_with("Development User API listening on ") {
                break;
            }
        }
    })
    .await
    .expect("actual listener");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if d.store
                .job(&d.owner, &job)
                .await
                .expect("query")
                .settlement_state
                == "settled"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("service maintenance, no manual expiry call");
    assert_eq!(
        d.store
            .job(&d.owner, &job)
            .await
            .expect("expired")
            .execution_state,
        "failed"
    );
    assert!(child.try_wait().expect("owned child").is_none());
    let pid = rustix::process::Pid::from_raw(child.id().expect("owned unreaped child") as i32)
        .expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::INT).expect("graceful stop");
    assert!(
        tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .expect("maintenance joined")
            .expect("exit")
            .success()
    );
}

#[tokio::test]
async fn bounded_scan_passes_persistent_failures_and_retries_them_after_wrap() {
    use orange_control_plane::store::QueueExpiryCursor;
    let a = device("client-eg").await;
    let b = device("client-eh").await;
    let c = device("client-ei").await;
    let first = execution::queued_with_deadline(&a, 2).await;
    let second = execution::queued_with_deadline(&b, 2).await;
    let good = execution::queued_with_deadline(&c, 2).await;
    for job in [&first, &second, &good] {
        due(&a.store, job).await;
    }
    let first_id = Uuid::parse_str(&first).expect("fixture ID");
    let second_id = Uuid::parse_str(&second).expect("fixture ID");
    let suffix = Uuid::new_v4().simple().to_string();
    let fault = format!(
        "CREATE FUNCTION expiry_scan_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM ledger_transactions WHERE transaction_id=NEW.transaction_id AND business_key IN('release:{first_id}','release:{second_id}')) THEN RAISE EXCEPTION 'explicit persistent scan fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER expiry_scan_fault_{suffix} BEFORE INSERT ON ledger_entries FOR EACH ROW EXECUTE FUNCTION expiry_scan_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(fault))
        .execute(a.store.pool())
        .await
        .expect("scoped faults");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs j JOIN quote_execution_plans p USING(quote_id) WHERE j.execution_state='queued' AND j.settlement_state='pending' AND p.job_deadline<=clock_timestamp()").fetch_one(a.store.pool()).await.expect("finite fixture backlog");
    let mut cursor = QueueExpiryCursor::default();
    let mut failed = 0;
    let mut scans_ok = true;
    for _ in 0..count + 2 {
        match a.store.expire_queued_ready(&mut cursor, 2).await {
            Ok((_, errors)) => failed += errors,
            Err(_) => {
                scans_ok = false;
                break;
            }
        }
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER expiry_scan_fault_{suffix} ON ledger_entries; DROP FUNCTION expiry_scan_fault_{suffix}();"))).execute(a.store.pool()).await.expect("remove exact faults");
    assert!(scans_ok);
    assert!(
        failed >= 2,
        "both persistent failures are attempted, not hidden"
    );
    assert_eq!(
        c.store
            .job(&c.owner, &good)
            .await
            .expect("later job")
            .settlement_state,
        "settled"
    );
    for (device, job) in [(&a, &first), (&b, &second)] {
        assert_eq!(
            device
                .store
                .job(&device.owner, job)
                .await
                .expect("failed release rolled back")
                .settlement_state,
            "pending"
        );
    }
    // Same cursor must eventually wrap and revisit both repaired candidates.
    for _ in 0..count + 2 {
        a.store
            .expire_queued_ready(&mut cursor, 2)
            .await
            .expect("retry scan");
    }
    for (device, job) in [(&a, &first), (&b, &second)] {
        assert_eq!(
            device
                .store
                .job(&device.owner, job)
                .await
                .expect("repaired candidate")
                .settlement_state,
            "settled"
        );
        let releases: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
                .bind(format!("release:{job}"))
                .fetch_one(a.store.pool())
                .await
                .expect("single release");
        assert_eq!(releases, 1);
    }
    assert!(matches!(
        a.store.expire_queued_ready(&mut cursor, 0).await,
        Err(StoreError::Invalid)
    ));
    assert!(matches!(
        a.store.expire_queued_ready(&mut cursor, 101).await,
        Err(StoreError::Invalid)
    ));
}
