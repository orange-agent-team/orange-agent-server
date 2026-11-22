//! Compare the marketplace journeys with daily merchant facts and the whole platform ledger.
use super::{Store, journey_merchant};
use orange_control_plane::payments::Payments;
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

fn integer(obj: &Value, key: &str) -> i128 {
    obj[key]
        .as_str()
        .expect("lossless report value")
        .parse()
        .expect("integer accounting value")
}

pub(super) async fn verify_days(
    db: &Store,
    payments: &Payments,
    team: &str,
    alice: &str,
    bob: &str,
) {
    let pool = db.pool();
    let merchant = journey_merchant::MERCHANT;
    let days:Vec<i64>=sqlx::query_scalar("SELECT DISTINCT floor(extract(epoch FROM recorded_at)/86400)::bigint*86400 AS day
        FROM test_statement_obligations WHERE merchant_id=$1 UNION SELECT DISTINCT floor(extract(epoch FROM d.committed_at)/86400)::bigint*86400
        FROM current_metered_deliveries d JOIN jobs j USING(job_id) WHERE (j.owner_id=$2 AND d.provider_owner_id=$3)
        OR (j.owner_id=$3 AND d.provider_owner_id=$4) ORDER BY 1")
        .bind(merchant).bind(team).bind(alice).bind(bob).fetch_all(pool).await.expect("actual UTC days");
    assert!(!days.is_empty());
    let postings = "SELECT count(*) FROM ledger_entries";
    let before: i64 = sqlx::query_scalar(postings)
        .fetch_one(pool)
        .await
        .expect("count pre-scan postings");
    let mut checked = Vec::new();
    let mut own = [0i128; 4];
    let mut other_jobs = 0i128;
    let mut business = BTreeMap::<(String, String), [i128; 3]>::new();
    for day in days {
        // Only close the reporting window early. Keep all actual business timestamps.
        let run = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO test_statement_runs(run_id,merchant_id,from_unix,until_unix) VALUES($1,$2,$3,$4)")
          .bind(&run).bind(merchant).bind(day).bind(day+86400).execute(pool).await.expect("daily window");
        sqlx::query("INSERT INTO test_statement_sync(merchant_id,run_id) VALUES($1,$2) ON CONFLICT(merchant_id) DO UPDATE
            SET run_id=excluded.run_id, next_check=clock_timestamp() WHERE test_statement_sync.run_id IS NULL")
           .bind(merchant).bind(&run).execute(pool).await.expect("start daily scan");
        for _ in 0..4 {
            assert!(
                payments
                    .scan_statement_page()
                    .await
                    .expect("scan real job receipts")
            );
        }
        let rows:Vec<(String,String,i64,bool)>=sqlx::query_as("SELECT owner_id,expected_provider,charge,verified FROM test_statement_job_checks WHERE run_id=$1")
            .bind(&run).fetch_all(pool).await.expect("scanned original jobs");
        checked.extend(rows);
        let report: Value =
            sqlx::query_scalar("SELECT report FROM test_statement_runs WHERE run_id=$1")
                .bind(&run)
                .fetch_one(pool)
                .await
                .expect("sealed daily report");
        assert_eq!(report["from_unix"], day.to_string());
        assert_eq!(report["until_unix"], (day + 86400).to_string());
        assert_eq!(report["observed_scan_issues"], "0", "{report}");
        for line in report["local_business_by_original_currency"]
            .as_array()
            .expect("local business totals")
        {
            let key = (
                line["currency"].as_str().expect("currency").to_owned(),
                line["flow"].as_str().expect("business flow").to_owned(),
            );
            let total = business.entry(key).or_default();
            for (index, name) in ["entries", "original_minor", "purchased_bucket_change"]
                .iter()
                .enumerate()
            {
                total[index] += integer(line, name);
            }
        }
        // Other tests can settle legitimate jobs for unrelated merchants on the same UTC day.
        // Compute their exact signed splits from the original ledger, independently of the report amounts.
        let (count,income,fee):(i64,String,String)=sqlx::query_as("SELECT count(DISTINCT s.job_id),
            coalesce(sum(e.amount::numeric) FILTER(WHERE e.account=s.provider_owner_id||':earned:pending'),0)::text,
            coalesce(sum(e.amount::numeric) FILTER(WHERE e.account='platform:fees'),0)::text
            FROM job_settlements s JOIN ledger_entries e USING(transaction_id) JOIN jobs j USING(job_id)
            WHERE s.settled_at>=to_timestamp($1::bigint) AND s.settled_at<to_timestamp($2::bigint)
              AND NOT ((j.owner_id=$3 AND s.provider_owner_id=$4) OR (j.owner_id=$4 AND s.provider_owner_id=$5))")
            .bind(day).bind(day+86400).bind(team).bind(alice).bind(bob).fetch_one(pool).await.expect("other settlement postings");
        let income: i128 = income.parse().expect("other net");
        let fee: i128 = fee.parse().expect("other fee");
        let jobs = &report["platform_wide_job_credits"];
        other_jobs += i128::from(count);
        for (index, (name, others)) in [
            ("settled_jobs", i128::from(count)),
            ("charged_microcredits", income + fee),
            ("provider_pending_microcredits", income),
            ("platform_fee_microcredits", fee),
        ]
        .iter()
        .enumerate()
        {
            own[index] += integer(jobs, name) - others;
        }
    }
    assert_eq!(checked.len(), 2);
    assert!(
        checked.contains(&(team.into(), alice.into(), 2200, true)),
        "{checked:?}"
    );
    assert!(
        checked.contains(&(alice.into(), bob.into(), 2200, true)),
        "{checked:?}"
    );
    assert_eq!(
        business,
        BTreeMap::from([
            (("usd".into(), "purchase".into()), [2, 1000, 20000]),
            (("usd".into(), "refund".into()), [1, -100, -2000])
        ])
    );
    assert_eq!(
        own,
        [2, 4400, 3520, 880],
        "contribution of the two real jobs after independently verifying {other_jobs} unrelated settlements"
    );
    let after: i64 = sqlx::query_scalar(postings)
        .fetch_one(pool)
        .await
        .expect("count post-scan postings");
    assert_eq!(before, after);
    println!("Reconciled two reciprocal jobs plus {other_jobs} unrelated daily settlements");
}
