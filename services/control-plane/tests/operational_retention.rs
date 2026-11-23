//! Runs in a database created just for this suite. Ages describe test observations, not elapsed wall time.
use orange_control_plane::{events::prune_old_notifications, store::Store};
use sqlx::PgPool;

async fn seed(pool: &PgPool, kind: &str, ages: &[i32], deliveries: &[Option<i32>]) -> String {
    assert_eq!(ages.len(), deliveries.len());
    let stream = uuid::Uuid::new_v4().to_string();
    let mut tx = pool.begin().await.expect("fixture transaction");
    sqlx::query("INSERT INTO event_streams(stream_id,owner_id,entity_kind,entity_id,next_seq) VALUES($1,'retention-owner',$2,$1,$3)")
        .bind(&stream).bind(kind).bind(ages.len() as i64+1).execute(&mut *tx).await.expect("stream");
    for (index, (&age, &delivery)) in ages.iter().zip(deliveries).enumerate() {
        let event = uuid::Uuid::new_v4().to_string();
        let receipt: i64 =
            sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('outbox','event_id'))")
                .fetch_one(&mut *tx)
                .await
                .expect("outbox sequence");
        sqlx::query("INSERT INTO durable_events(event_id,stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id)
            VALUES($1,$2,$3,$4,encode(sha256(convert_to('fixture','UTF8')),'hex'),$2,clock_timestamp()-make_interval(days=>$5),$6)")
            .bind(&event).bind(&stream).bind(index as i64+1).bind(format!("{kind}.fixture")).bind(age).bind(receipt)
            .execute(&mut *tx).await.expect("event");
        sqlx::query("INSERT INTO outbox(event_id,owner_id,entity_id,event_type,payload,domain_event_id,created_at,delivered_at)
            VALUES($1,'retention-owner',$2,$3,'fixture',$4,clock_timestamp()-make_interval(days=>$5),
                CASE WHEN $6::integer IS NULL THEN NULL ELSE clock_timestamp()-make_interval(days=>$6) END)")
            .bind(receipt).bind(&stream).bind(format!("{kind}.fixture")).bind(&event).bind(age).bind(delivery)
            .execute(&mut *tx).await.expect("matching envelope");
    }
    tx.commit().await.expect("persist fixture");
    stream
}

#[tokio::test]
async fn aged_notification_prefix_preserves_recent_pending_and_independent_financial_facts() {
    let store = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("owned database"))
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = store.pool();
    sqlx::query("INSERT INTO owners(owner_id) VALUES('retention-owner')")
        .execute(pool)
        .await
        .expect("owner");
    sqlx::query("INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES('protected-transaction','original-retention-fixture','retention-owner')")
        .execute(pool).await.expect("financial fixture");
    let job = seed(
        pool,
        "job",
        &[46, 45, 1, 61],
        &[Some(45), Some(40), Some(1), Some(60)],
    )
    .await;
    let worker = seed(pool, "worker", &[42, 40, 50], &[Some(41), None, Some(49)]).await;
    let node = seed(pool, "node", &[40, 60], &[Some(2), Some(59)]).await;
    let financial = seed(pool, "wallet", &[50], &[Some(45)]).await;
    let audit = seed(pool, "admin", &[50], &[Some(45)]).await;
    let damaged = seed(pool, "job", &[45, 44], &[Some(43), Some(42)]).await;
    sqlx::query("DELETE FROM outbox WHERE domain_event_id=(SELECT event_id FROM durable_events WHERE stream_id=$1 AND seq=2)")
        .bind(&damaged).execute(pool).await.expect("local damaged handoff fixture");
    let run = || prune_old_notifications(pool, 8, 3);
    let (one, two) = tokio::join!(run(), run());
    let (one, _) = one.expect("first maintenance");
    let (two, _) = two.expect("second maintenance");
    assert_eq!(one + two, 3);
    assert_eq!(run().await.expect("repeat"), (0, 1));
    for (stream, oldest, count) in [
        (&job, 3, 2),
        (&worker, 2, 2),
        (&node, 1, 2),
        (&financial, 1, 1),
        (&audit, 1, 1),
        (&damaged, 1, 2),
    ] {
        let actual:(i64,i64)=sqlx::query_as("SELECT oldest_seq,(SELECT count(*) FROM durable_events WHERE stream_id=$1) FROM event_streams WHERE stream_id=$1")
            .bind(stream).fetch_one(pool).await.expect("retained head");
        assert_eq!(actual, (oldest, count));
    }
    let postings: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ledger_transactions WHERE transaction_id='protected-transaction'",
    )
    .fetch_one(pool)
    .await
    .expect("unaltered ledger");
    assert_eq!(postings, 1);
    assert!(prune_old_notifications(pool, 33, 1).await.is_err());
    assert!(prune_old_notifications(pool, 1, 257).await.is_err());
    let bounded = seed(
        pool,
        "worker",
        &[40, 40, 40],
        &[Some(40), Some(40), Some(40)],
    )
    .await;
    let (removed, _) = prune_old_notifications(pool, 8, 1)
        .await
        .expect("bounded maintenance");
    // The intact first row of the damaged stream can be pruned when the selected
    // prefix contains only that row. The missing second handoff itself is never removed.
    assert!((1..=2).contains(&removed));
    let head: i64 = sqlx::query_scalar("SELECT oldest_seq FROM event_streams WHERE stream_id=$1")
        .bind(bounded)
        .fetch_one(pool)
        .await
        .expect("bounded head");
    assert_eq!(head, 2);
}
