//! Build a de-identified paid cohort from real merchant and immutable ledger receipts in the owned DB.
use orange_control_plane::market_metrics::keys::{Entity, ExportKeys};
use orange_control_plane::market_metrics::{Snapshot, report};
use serde_json::{Value, json};

use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub async fn verify(
    pool: &PgPool,
    merchant: &str,
    payers: &[&str],
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut secret = [0; 32];
    secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let keys = ExportKeys::new(&secret)?;
    let owners: Vec<String> = payers.iter().map(|s| s.to_string()).collect();
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM ledger_entries")
        .fetch_one(&mut *tx)
        .await?;
    let mut facts: Vec<Value> = vec![];
    let mut event = |domain: &str, id: &str, at: i64, fact: Value| {
        facts.push(json!({"receipt":keys.receipt(domain,id).expect("fixture receipt id"),"at":at,"fact":fact}))
    };
    let flows=sqlx::query("SELECT o.flow,o.business_id,o.owner_id,o.currency,o.gross,o.units,
        floor(extract(epoch FROM o.recorded_at))::bigint AS at,coalesce(r.order_id,o.business_id) AS original
        FROM test_statement_obligations o LEFT JOIN test_refund_orders r ON o.flow='refund' AND r.refund_id=o.business_id
        WHERE o.merchant_id=$1 AND o.owner_id=ANY($2)").bind(merchant).bind(&owners).fetch_all(&mut *tx).await?;
    for row in flows {
        let flow: &str = row.try_get("flow")?;
        let id: &str = row.try_get("business_id")?;
        let minor: i64 = row.try_get("gross")?;
        let units: i64 = row.try_get("units")?;
        let fact = match flow {
            "purchase" => {
                json!({"type":"purchase","order":keys.subject(Entity::Order,id),"payer":keys.subject(Entity::Payer,row.try_get("owner_id")?),"currency":row.try_get::<&str,_>("currency")?,"minor":minor.to_string(),"units":units.to_string()})
            }
            "refund" => {
                json!({"type":"refund","refund":keys.subject(Entity::Refund,id),"order":keys.subject(Entity::Order,row.try_get("original")?),"minor":(-i128::from(minor)).to_string(),"units":(-i128::from(units)).to_string()})
            }
            _ => return Err("unexpected verified merchant obligation".into()),
        };
        event(flow, id, row.try_get("at")?, fact);
    }
    let rows=sqlx::query("SELECT j.job_id,j.owner_id,s.transaction_id,s.charge,s.platform_fee,s.provider_pending,
        floor(extract(epoch FROM r.created_at))::bigint AS submitted,floor(extract(epoch FROM c.committed_at))::bigint AS finished,
        floor(extract(epoch FROM s.settled_at))::bigint AS settled
        FROM jobs j JOIN job_settlements s USING(job_id) JOIN job_response_commits c USING(job_id)
        JOIN ledger_transactions r ON r.business_key='reserve:'||j.job_id AND r.owner_id=j.owner_id
        WHERE j.owner_id=ANY($1)").bind(&owners).fetch_all(&mut *tx).await?;
    for row in rows {
        let id: &str = row.try_get("job_id")?;
        let owner: &str = row.try_get("owner_id")?;
        let job = keys.subject(Entity::Job, id);
        event(
            "submission",
            id,
            row.try_get("submitted")?,
            json!({"type":"job_submitted","job":job,"payer":keys.subject(Entity::Payer,owner)}),
        );
        event(
            "delivery",
            id,
            row.try_get("finished")?,
            json!({"type":"job_finished","job":job,"outcome":"delivered","winner":null}),
        );
        let transaction: &str = row.try_get("transaction_id")?;
        let buckets:(i64,i64,i64,i64)=sqlx::query_as("SELECT
          coalesce(-sum(amount) FILTER (WHERE account IN ($1||':gift:available',$1||':gift:reserved')),0)::bigint,
          coalesce(-sum(amount) FILTER (WHERE account IN ($1||':earned:available',$1||':earned:reserved')),0)::bigint,
          coalesce(-sum(amount) FILTER (WHERE account IN ($1||':purchased:available',$1||':purchased:reserved')),0)::bigint,
          sum(amount)::bigint FROM ledger_entries WHERE transaction_id=$2").bind(owner).bind(transaction).fetch_one(&mut *tx).await?;
        assert_eq!(buckets.3, 0, "actual double-entry settlement");
        let used:Vec<(String,i64)>=sqlx::query_as("SELECT a.order_id,a.charged FROM test_job_lot_allocations a JOIN test_checkout_orders o USING(order_id)
            WHERE a.job_id=$1 AND o.merchant_id=$2 AND a.charged>0 ORDER BY a.order_id").bind(id).bind(merchant).fetch_all(&mut *tx).await?;
        let lots = used
            .iter()
            .map(|(id, n)| json!({"order":keys.subject(Entity::Order,id),"units":n.to_string()}))
            .collect::<Vec<_>>();
        event(
            "settlement",
            transaction,
            row.try_get("settled")?,
            json!({"type":"settlement","job":job,"charge":row.try_get::<i64,_>("charge")?.to_string(),
            "gift":buckets.0.to_string(),"earned":buckets.1.to_string(),"purchased":buckets.2.to_string(),"lots":lots,
            "platform_fee":row.try_get::<i64,_>("platform_fee")?.to_string(),"provider_pending":row.try_get::<i64,_>("provider_pending")?.to_string()}),
        );
    }
    let releases:Vec<(String,String,i64,i64)>=sqlx::query_as("SELECT e.job_id,r.transaction_id,e.units,floor(extract(epoch FROM r.released_at))::bigint
        FROM provider_earning_releases r JOIN provider_earning_origins e USING(job_id) WHERE e.buyer_owner=ANY($1)").bind(&owners).fetch_all(&mut *tx).await?;
    for (job, transaction, units, at) in releases {
        event(
            "provider-release",
            &transaction,
            at,
            json!({"type":"earning_released","job":keys.subject(Entity::Job,&job),"units":units.to_string()}),
        );
    }
    let start = facts
        .iter()
        .filter_map(|v| v["at"].as_i64())
        .min()
        .ok_or("empty cohort")?;
    let until = facts
        .iter()
        .filter_map(|v| v["at"].as_i64())
        .max()
        .ok_or("empty cohort")?
        + 1;
    let extract = json!({"version":1,"population":"settled_payer_cohort","supply_history_complete":false,"earning_releases_complete":true,"from_unix":start,"until_unix":until,"as_of_unix":until,
        "cost_coverage":[],"events":facts});
    let snapshot: Snapshot = serde_json::from_value(extract.clone())?;
    let actual = report(&snapshot)?;
    assert!(actual["supply"].is_null());
    assert_eq!(actual["demand"]["settled"], 2);
    assert!(actual["demand"]["failed"].is_null());
    assert_eq!(actual["microcredits"]["purchased_used"], "4400");
    assert_eq!(actual["microcredits"]["provider_pending"], "3520");
    assert_eq!(actual["microcredits"]["provider_released"], "0");
    assert_eq!(actual["microcredits"]["platform_fee"], "880");
    let cash = &actual["currency_totals"][0];
    assert_eq!(cash["currency"], "usd");
    assert_eq!(cash["purchase_minor"], "1000");
    assert_eq!(cash["refund_minor"], "100");
    assert_eq!(cash["attributed_service_minor"], "220");
    assert!(
        cash["contribution_minor"].is_null(),
        "no independent cost observations in real cohort"
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM ledger_entries")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(before, after);
    tx.commit().await?;
    std::fs::write(
        output.join("paid-cohort.snapshot.json"),
        serde_json::to_vec_pretty(&extract)?,
    )?;
    std::fs::write(
        output.join("paid-cohort.report.json"),
        serde_json::to_vec_pretty(&actual)?,
    )?;
    println!(
        "PASS read-only marketplace cohort: two purchases, one refund, two CPU jobs, exact original lots, balanced postings, missing cash costs reported as unknown"
    );
    Ok(())
}
