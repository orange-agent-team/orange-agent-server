use super::super::host_cases as host;
use super::*;
fn enable_cleanup(path: &std::path::Path) {
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("config")).expect("JSON");
    v["confirm_cleanup"] = serde_json::json!(true);
    std::fs::write(path, serde_json::to_vec(&v).expect("JSON")).expect("private config");
}
async fn assert_terminal(c: &Case) {
    let state: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&c.f.job)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("attempt");
    assert_eq!(state, "closed");
    let pins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact_pins WHERE reference_id IN($1,$2)")
            .bind(format!("job-input:{}", c.f.job))
            .bind(format!("job-output:{}", c.f.job))
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("pins");
    assert_eq!(pins, 0);
}
#[tokio::test]
async fn actual_cleanup_allows_second_priced_model_job_without_replaying_first() {
    let mut c = setup_priced("client-cs").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let root = runtime();
    let path = host::configuration_port(&c.f, server.port, &profile(), root.path());
    host::enable_outputs(&path);
    enable_cleanup(&path);
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    let first = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &first).await;
    host::event(&mut events, "response_committed").await;
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    assert_terminal(&c).await;
    assert_eq!(
        c.f.d
            .store
            .job(&c.f.owner, &c.f.job)
            .await
            .expect("first job")
            .settlement_state,
        "settled"
    );
    let first_job = c.f.job.clone();
    let prior: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("model/process.json")).expect("first instance"),
    )
    .expect("JSON");
    replace_input(&mut c).await;
    let second = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &second).await;
    host::event(&mut events, "response_committed").await;
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    assert_terminal(&c).await;
    let current: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("model/process.json")).expect("second instance"),
    )
    .expect("JSON");
    assert_ne!(prior["instance_id"], current["instance_id"]);
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worker_cpu_cleanup_receipts WHERE job_id IN($1,$2)",
    )
    .bind(&first_job)
    .bind(&c.f.job)
    .fetch_one(c.f.d.store.pool())
    .await
    .expect("two receipts");
    assert_eq!(receipts, 2);
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("execution.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("local history");
    let facts: i64 = db
        .query_row(
            "SELECT count(*) FROM cpu_cleanup_facts WHERE receipt IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("facts");
    assert_eq!(facts, 2);
    drop(db);
    host::terminate(&mut child).await;
    host::stopped(root.path());
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("meter stopped");
}
#[tokio::test]
async fn actual_undelivered_output_cleanup_releases_original_hold_without_fake_usage() {
    let mut c = setup_priced("client-ct").await;
    c.f.request.messages = vec![Message::plain(
        Role::User,
        "Output the words orange apple banana grape melon.".into(),
    )];
    c.f.request.max_output_tokens = 64;
    replace_input(&mut c).await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let root = runtime();
    let path = host::configuration_port(&c.f, server.port, &profile(), root.path());
    host::enable_outputs(&path);
    enable_cleanup(&path);
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    let q = quote_http(&c, &quote_request(&c, 1)).await;
    submit(&mut c, &q).await;
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    assert_terminal(&c).await;
    let job =
        c.f.d
            .store
            .job(&c.f.owner, &c.f.job)
            .await
            .expect("terminal");
    assert_eq!(job.execution_state, "failed");
    assert_eq!(job.settlement_state, "settled");
    assert_eq!(job.charge.expect("zero due to no delivery").0.units(), 0);
    let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM job_usage_facts WHERE job_id=$1")
        .bind(&c.f.job)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("no invented zero measurement");
    assert_eq!(facts, 0);
    let wallet: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&c.f.owner)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("original hold restored");
    assert_eq!(wallet, (1000, 0));
    let releases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM job_cleanup_releases WHERE job_id=$1")
            .bind(&c.f.job)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("one release");
    assert_eq!(releases, 1);
    host::terminate(&mut child).await;
    host::stopped(root.path());
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("meter stopped");
}

#[tokio::test]
async fn actual_cleanup_receipt_recovers_without_new_model_and_deleted_source_reclaims_derivative()
{
    let mut c = setup_priced("client-cx").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let root = runtime();
    let path = host::configuration_port(&c.f, server.port, &profile(), root.path());
    host::enable_outputs(&path);
    enable_cleanup(&path);
    let (mut child, mut events) = host::start(&path);
    host::event(&mut events, "ready").await;
    let fault = rusqlite::Connection::open(root.path().join("execution.sqlite"))
        .expect("isolated local receipt fault");
    fault.execute_batch("CREATE TRIGGER cleanup_receipt_fault BEFORE UPDATE ON cpu_cleanup_facts WHEN NEW.receipt IS NOT NULL BEGIN SELECT RAISE(ABORT,'explicit cleanup receipt failure'); END;").expect("fault");
    let q = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &q).await;
    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("bounded failed receipt")
        .expect("wait");
    assert!(!status.success());
    host::stopped(root.path());
    assert_terminal(&c).await;
    let prior = std::fs::read(root.path().join("model/process.json")).expect("first stopped model");
    fault
        .execute_batch("DROP TRIGGER cleanup_receipt_fault;")
        .expect("repair exact fault");
    drop(fault);
    let (mut recovered, mut events) = host::start(&path);
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    host::event(&mut events, "ready").await;
    assert_eq!(
        prior,
        std::fs::read(root.path().join("model/process.json")).expect("no model replay")
    );
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_cpu_cleanup_receipts WHERE job_id=$1")
            .bind(&c.f.job)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("one receipt");
    assert_eq!(receipts, 1);
    let result =
        c.f.objects
            .job_result(&c.f.actor, &c.f.owner, &c.f.job)
            .await
            .expect("original result");
    sqlx::query(
        "INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,'other-consumer-fixture')",
    )
    .bind(&result.artifact_id)
    .execute(c.f.d.store.pool())
    .await
    .expect("independent consumer");
    let key:String=sqlx::query_scalar("SELECT t.published_key FROM artifacts a JOIN artifact_transfers t ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1").bind(&result.artifact_id).fetch_one(c.f.d.store.pool()).await.expect("exact blob");
    c.f.objects
        .delete(&c.f.actor, &c.f.owner, &c.f.upload.upload_id)
        .await
        .expect("source delete");
    c.f.objects
        .collect(100)
        .await
        .expect("lineage expiry and stage cleanup");
    let deleted: String = sqlx::query_scalar("SELECT state FROM artifacts WHERE artifact_id=$1")
        .bind(&result.artifact_id)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("derivative tombstone");
    assert_eq!(deleted, "deleted");
    assert!(
        c.f.objects
            .job_result(&c.f.actor, &c.f.owner, &c.f.job)
            .await
            .is_err()
    );
    sqlx::query("UPDATE artifacts SET deleted_at=clock_timestamp()-interval '25 hours' WHERE artifact_id=$1").bind(&result.artifact_id).execute(c.f.d.store.pool()).await.expect("retention clock fixture");
    c.f.objects.collect(100).await.expect("pin protects bytes");
    assert!(c.f.root.join(&key).is_file());
    sqlx::query(
        "DELETE FROM artifact_pins WHERE artifact_id=$1 AND reference_id='other-consumer-fixture'",
    )
    .bind(&result.artifact_id)
    .execute(c.f.d.store.pool())
    .await
    .expect("release only fixture consumer");
    c.f.objects
        .collect(100)
        .await
        .expect("actual reclaim after final pin");
    assert!(!c.f.root.join(key).exists());
    host::terminate(&mut recovered).await;
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("meter stopped");
}
