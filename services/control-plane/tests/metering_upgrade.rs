//! Migration from real schema-64 posting facts. The owning script creates and drops this database.
#![cfg(feature = "integration-tests")]
use orange_control_plane::store::Store;
use sqlx::PgPool;

async fn balances(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT name,md5(string_agg(fact,'|' ORDER BY fact)) FROM (
      SELECT 'wallet' AS name,to_jsonb(w)::text AS fact FROM wallets w
      UNION ALL SELECT 'budget',to_jsonb(b)::text FROM budget_roots b
      UNION ALL SELECT 'transaction',to_jsonb(t)::text FROM ledger_transactions t
      UNION ALL SELECT 'posting',to_jsonb(e)::text FROM ledger_entries e
      UNION ALL SELECT 'settlement',to_jsonb(s)::text FROM job_settlements s) facts GROUP BY name ORDER BY name")
      .fetch_all(pool).await.expect("capture account and ledger facts")
}
#[tokio::test]
async fn existing_postings_keep_original_delivery_identity_and_money_after_upgrade() {
    let url = std::env::var("ORANGE_METER_UPGRADE_DATABASE").expect("owned prior-version fixture");
    let db = Store::connect(&url)
        .await
        .expect("connect old-version database");
    let pool = db.pool();
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await
            .expect("read migration version");
    assert_eq!(version, 64);
    let before = balances(pool).await;
    assert_eq!(before.len(), 5);
    let prior: i64 = sqlx::query_scalar("SELECT count(*) FROM job_settlements")
        .fetch_one(pool)
        .await
        .expect("count original settlements");
    assert!(
        prior >= 2,
        "prior binary must have posted real metered jobs"
    );
    db.migrate().await.expect("apply pending migration");
    let matching:i64=sqlx::query_scalar("SELECT count(*) FROM metering_receipts r JOIN job_settlements s USING(job_id)
       JOIN job_response_commits c USING(job_id) JOIN jobs j USING(job_id)
       WHERE r.event_id=c.commit_id AND r.response_digest=c.response_digest AND r.owner_id=j.owner_id AND r.received_at=c.committed_at
       AND r.transaction_id=s.transaction_id AND r.state='posted' AND
       (SELECT array_agg(stage ORDER BY sequence_id) FROM metering_receipt_history WHERE event_id=r.event_id)=ARRAY['received','validated','posted']::text[]")
       .fetch_one(pool).await.expect("match migrated receipt trail");
    assert_eq!(matching, prior);
    db.migrate().await.expect("repeat migrations idempotently");
    assert_eq!(balances(pool).await, before);
    println!(
        "Matched {matching} original settlements and compared all wallet, budget and ledger facts"
    );
    pool.close().await;
}
