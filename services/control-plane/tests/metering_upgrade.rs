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
    let expected = std::env::var("ORANGE_METER_PRIOR_VERSION")
        .ok()
        .map_or(64, |n| n.parse::<i64>().expect("known prior schema"));
    assert!([64, 65].contains(&expected));
    assert_eq!(version, expected);
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
    if expected == 64 {
        assert_eq!(matching, prior);
    } else {
        // Schema 65 also contains correctly recovered receipts with multiple attempts and an optional review.
        let all:i64=sqlx::query_scalar("SELECT count(*) FROM metering_receipts r JOIN job_settlements s USING(job_id) JOIN job_response_commits c USING(job_id)
          WHERE r.transaction_id=s.transaction_id AND r.event_id=c.commit_id AND r.state='posted'").fetch_one(pool).await.expect("all prior metered settlements");
        assert_eq!(all, prior);
        let origins:(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE s.provider_pending>0),count(e.job_id) FILTER(WHERE e.units=s.provider_pending AND e.source_transaction=s.transaction_id
         AND e.provider_owner=s.provider_owner_id AND e.settled_at=s.settled_at AND e.eligible_at=s.settled_at+interval '24 hours') FROM job_settlements s LEFT JOIN provider_earning_origins e USING(job_id)")
         .fetch_one(pool).await.expect("original earning entitlements");
        assert!(origins.0 > 1);
        assert_eq!(origins.0, origins.1);
        let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_earning_releases")
            .fetch_one(pool)
            .await
            .expect("no maturity on migration");
        assert_eq!(releases, 0);
        println!(
            "Matched {} prior positive provider credits without releasing a balance",
            origins.1
        );
    }
    db.migrate().await.expect("repeat migrations idempotently");
    assert_eq!(balances(pool).await, before);
    println!(
        "Matched {matching} original settlements and compared all wallet, budget and ledger facts"
    );
    pool.close().await;
}
