//! Read-only financial comparison. Only scan metadata, immutable observations and operator cases are written.
use super::{Error, Payments, Sandbox, stripe::BalanceLine};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
type Tx<'a> = Transaction<'a, Postgres>;
const DAY: i64 = 86400;

struct Claim {
    run: String,
    merchant: String,
    from: i64,
    until: i64,
    phase: String,
    cursor: String,
    generation: i64,
}
async fn claim(pool: &PgPool, merchant: &str) -> Result<Option<Claim>, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO test_statement_sync(merchant_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(merchant)
        .execute(&mut *tx)
        .await?;
    let r=sqlx::query("SELECT high_water,activation_day,run_id,phase,page_cursor,
        (floor(extract(epoch FROM clock_timestamp())/86400)::bigint*86400) AS today
        FROM test_statement_sync WHERE merchant_id=$1 AND next_check<=clock_timestamp() AND (claim_until IS NULL OR claim_until<=clock_timestamp()) FOR UPDATE SKIP LOCKED")
        .bind(merchant).fetch_optional(&mut *tx).await?;
    let Some(r) = r else { return Ok(None) };
    let today: i64 = r.try_get("today")?;
    let (run, from, until) = if let Some(id) = r.try_get::<Option<String>, _>("run_id")? {
        let (from,until)=sqlx::query_as("SELECT from_unix,until_unix FROM test_statement_runs WHERE run_id=$1 AND merchant_id=$2 AND completed_at IS NULL").bind(&id).bind(merchant).fetch_one(&mut *tx).await?;
        (id, from, until)
    } else {
        let high: Option<i64> = r.try_get("high_water")?;
        let first:Option<i64>=sqlx::query_scalar("SELECT floor(extract(epoch FROM min(created_at))/86400)::bigint*86400 FROM test_checkout_orders WHERE merchant_id=$1")
            .bind(merchant).fetch_one(&mut *tx).await?;
        let activation: i64 = r.try_get("activation_day")?;
        let first = first.map_or(activation, |order_day| activation.min(order_day));
        if high.unwrap_or(first) >= today {
            sqlx::query("UPDATE test_statement_sync SET next_check=clock_timestamp()+interval '1 hour' WHERE merchant_id=$1").bind(merchant).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(None);
        };
        let from = high.map_or(first, |h| (h - DAY).max(first));
        let until = high.unwrap_or(first).saturating_add(DAY).min(today);
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO test_statement_runs(run_id,merchant_id,from_unix,until_unix) VALUES($1,$2,$3,$4)").bind(&id).bind(merchant).bind(from).bind(until).execute(&mut *tx).await?;
        (id, from, until)
    };
    let generation=sqlx::query_scalar("UPDATE test_statement_sync SET run_id=$2,generation=generation+1,claim_until=clock_timestamp()+interval '60 seconds' WHERE merchant_id=$1 RETURNING generation")
        .bind(merchant).bind(&run).fetch_one(&mut *tx).await?;
    let c = Claim {
        run,
        merchant: merchant.to_owned(),
        from,
        until,
        phase: r.try_get("phase")?,
        cursor: r.try_get("page_cursor")?,
        generation,
    };
    tx.commit().await?;
    Ok(Some(c))
}

async fn issue(
    tx: &mut Tx<'_>,
    c: &Claim,
    kind: &str,
    business: &str,
    owner: Option<&str>,
    detail: Value,
) -> Result<(), Error> {
    let key = orange_protocol::request_digest("statement.issue.v1", &(&c.merchant, kind, business))
        .map_err(|_| Error::Mismatch)?;
    let digest = orange_protocol::request_digest("statement.observation.v1", &detail)
        .map_err(|_| Error::Mismatch)?;
    sqlx::query("INSERT INTO test_statement_issues(merchant_id,issue_key,kind,business_ref,owner_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(&c.merchant).bind(&key).bind(kind).bind(business).bind(owner).execute(&mut **tx).await?;
    let row=sqlx::query("SELECT owner_id,case_id FROM test_statement_issues WHERE merchant_id=$1 AND issue_key=$2 FOR UPDATE").bind(&c.merchant).bind(&key).fetch_one(&mut **tx).await?;
    if let Some(owner) = owner {
        if row
            .try_get::<Option<&str>, _>("owner_id")?
            .is_some_and(|s| s != owner)
        {
            return Err(Error::Mismatch);
        }
        if row.try_get::<Option<&str>, _>("case_id")?.is_none() {
            let case = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at) VALUES($1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint)")
                .bind(&case).bind(owner).bind(format!("test-statement:{key}")).execute(&mut **tx).await?;
            sqlx::query("UPDATE test_statement_issues SET case_id=$3,owner_id=$4 WHERE merchant_id=$1 AND issue_key=$2").bind(&c.merchant).bind(&key).bind(case).bind(owner).execute(&mut **tx).await?;
        }
    }
    sqlx::query("INSERT INTO test_statement_issue_evidence(run_id,merchant_id,issue_key,evidence_digest,detail) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(&c.run).bind(&c.merchant).bind(key).bind(digest).bind(detail).execute(&mut **tx).await?;
    Ok(())
}

fn family(kind: &str) -> Option<&'static str> {
    match kind {
        "charge" | "payment" => Some("purchase"),
        "refund" | "payment_refund" => Some("refund"),
        _ => None,
    }
}
async fn compare(tx: &mut Tx<'_>, c: &Claim, ob: &PgRow) -> Result<(), Error> {
    let source: &str = ob.try_get("source_id")?;
    let owner: &str = ob.try_get("owner_id")?;
    let business: &str = ob.try_get("business_id")?;
    let flow: &str = ob.try_get("flow")?;
    let gross: i64 = ob.try_get("gross")?;
    let currency: &str = ob.try_get("currency")?;
    let rows=sqlx::query("SELECT line_id,kind,gross,currency FROM test_statement_lines WHERE merchant_id=$1 AND source_id=$2 AND
        (CASE WHEN $3='purchase' THEN kind IN('charge','payment') ELSE kind IN('refund','payment_refund') END) ORDER BY created_unix,line_id LIMIT 2")
        .bind(&c.merchant).bind(source).bind(flow).fetch_all(&mut **tx).await?;
    let detail = json!({"business_id":business,"source":source,"original_currency":currency,"expected_minor":gross.to_string(),"flow":flow});
    match rows.as_slice(){
        []=>issue(tx,c,"missing_external",business,Some(owner),detail.clone()).await?,
        [one]=>{
            let cur:&str=one.try_get("currency")?;let amount:i64=one.try_get("gross")?;
            let code=if cur!=currency{Some("cross_currency_review")}else if amount!=gross{Some("amount_mismatch")}else{None};
            if let Some(code)=code{issue(tx,c,code,business,Some(owner),json!({"expected":detail,"observed_minor":amount.to_string(),"settlement_currency":cur,"line_id":one.try_get::<&str,_>("line_id")?})).await?;}
        }
        _=>issue(tx,c,"multiple_external",business,Some(owner),json!({"expected":detail,"first_two_lines":[rows[0].try_get::<&str,_>("line_id")?,rows[1].try_get::<&str,_>("line_id")?]})).await?
    }
    let id: &str = ob.try_get("transaction_id")?;
    let units: i64 = ob.try_get("units")?;
    let (key, account, delta) = if flow == "purchase" {
        (
            format!("test-checkout:{business}"),
            format!("{owner}:purchased:available"),
            units,
        )
    } else {
        (
            format!("test-refund-end:{business}"),
            format!("{owner}:purchased:refund-held"),
            units,
        )
    };
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ledger_transactions t WHERE t.transaction_id=$1 AND t.owner_id=$2 AND t.business_key=$3 AND
        (SELECT sum(amount::numeric) FROM ledger_entries WHERE transaction_id=t.transaction_id)=0 AND EXISTS(SELECT 1 FROM ledger_entries WHERE transaction_id=t.transaction_id AND account=$4 AND amount=$5)
        AND EXISTS(SELECT 1 FROM ledger_entries WHERE transaction_id=t.transaction_id AND account=$6 AND amount::numeric=-$5::bigint::numeric))")
        .bind(id).bind(owner).bind(key).bind(account).bind(delta).bind(format!("stripe-test:{}:clearing",c.merchant)).fetch_one(&mut **tx).await?;
    if !valid {
        issue(
            tx,
            c,
            "credit_posting_mismatch",
            business,
            Some(owner),
            json!({"transaction_id":id,"source":source,"expected_microcredits":units.to_string()}),
        )
        .await?;
    }
    Ok(())
}
async fn ingest(tx: &mut Tx<'_>, c: &Claim, line: &BalanceLine) -> Result<(), Error> {
    let inserted=sqlx::query("INSERT INTO test_statement_lines(merchant_id,line_id,source_id,kind,category,created_unix,currency,gross,fee,net)
       VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING").bind(&c.merchant).bind(&line.id).bind(&line.source).bind(&line.kind).bind(&line.reporting_category).bind(line.created)
       .bind(&line.currency).bind(line.amount).bind(line.fee).bind(line.net).execute(&mut **tx).await?.rows_affected();
    if inserted == 0 {
        let matches:bool=sqlx::query_scalar("SELECT source_id IS NOT DISTINCT FROM $3 AND kind=$4 AND category=$5 AND created_unix=$6 AND currency=$7 AND gross=$8 AND fee=$9 AND net=$10 FROM test_statement_lines WHERE merchant_id=$1 AND line_id=$2")
            .bind(&c.merchant).bind(&line.id).bind(&line.source).bind(&line.kind).bind(&line.reporting_category).bind(line.created).bind(&line.currency).bind(line.amount).bind(line.fee).bind(line.net).fetch_one(&mut **tx).await?;
        if !matches {
            issue(
                tx,
                c,
                "changed_external_line",
                &line.id,
                None,
                json!({"id":line.id,"object":"balance_transaction","source":line.source,"created":line.created,"currency":line.currency,"amount":line.amount.to_string(),"fee":line.fee.to_string(),"net":line.net.to_string(),"type":line.kind,"reporting_category":line.reporting_category,"status":line.status}),
            )
            .await?;
        }
    }
    let seen=sqlx::query("INSERT INTO test_statement_run_lines(run_id,merchant_id,line_id,provider_status,created_unix,currency,category,gross,fee,net) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING")
        .bind(&c.run).bind(&c.merchant).bind(&line.id).bind(&line.status).bind(line.created).bind(&line.currency).bind(&line.reporting_category).bind(line.amount).bind(line.fee).bind(line.net).execute(&mut **tx).await?.rows_affected();
    if seen != 1 {
        return Err(Error::Mismatch);
    }
    if let Some(flow) = family(&line.kind) {
        let source = line.source.as_deref();
        let ob=sqlx::query("SELECT * FROM test_statement_obligations WHERE merchant_id=$1 AND source_id=$2 AND flow=$3").bind(&c.merchant).bind(source).bind(flow).fetch_optional(&mut **tx).await?;
        if let Some(ob) = ob {
            compare(tx, c, &ob).await?;
        } else if let Some(refund)=sqlx::query("SELECT r.refund_id,r.owner_id,r.state FROM test_refund_orders r JOIN test_checkout_orders o USING(order_id)
            WHERE o.merchant_id=$1 AND r.provider_refund_id=$2 AND $3='refund'").bind(&c.merchant).bind(source).bind(flow).fetch_optional(&mut **tx).await?{
            issue(tx,c,"external_refund_without_posting",refund.try_get("refund_id")?,Some(refund.try_get("owner_id")?),json!({"source":source,"line_id":line.id,"local_state":refund.try_get::<&str,_>("state")?})).await?;
        } else {
            issue(tx,c,"unattributed_external",&line.id,None,json!({"source":source,"line_id":line.id,"currency":line.currency,"gross_minor":line.amount.to_string()})).await?;
        }
    }
    Ok(())
}
fn correct_postings(
    entries: &std::collections::BTreeMap<&str, i64>,
    owner: &str,
    supplier: &str,
    buckets: [i64; 3],
    charge: i64,
    fee: i64,
    income: i64,
) -> bool {
    let Ok(used) = crate::store::allocate(charge, buckets) else {
        return false;
    };
    entries.len() == 8
        && i128::from(charge) == i128::from(fee) + i128::from(income)
        && entries.get(format!("{supplier}:earned:pending").as_str()) == Some(&income)
        && entries.get("platform:fees") == Some(&fee)
        && ["gift", "earned", "purchased"]
            .iter()
            .zip(buckets)
            .zip(used)
            .all(|((name, held), charged)| {
                entries.get(format!("{owner}:{name}:reserved").as_str()) == Some(&(-held))
                    && entries.get(format!("{owner}:{name}:available").as_str())
                        == Some(&(held - charged))
            })
}

/// The merchant drilldown covers jobs with a reservation attributed to this merchant's purchased lots.
/// A settlement contains the whole job, even when other sources contributed to its reservation.
async fn check_job(tx: &mut Tx<'_>, c: &Claim, r: &PgRow) -> Result<(), Error> {
    let job: &str = r.try_get("job_id")?;
    let owner: &str = r.try_get("owner_id")?;
    let wanted: &str = r.try_get("expected_provider")?;
    let Some(transaction) = r.try_get::<Option<&str>, _>("transaction_id")? else {
        sqlx::query("INSERT INTO test_statement_job_checks(run_id,job_id,owner_id,expected_provider,verified) VALUES($1,$2,$3,$4,false)")
            .bind(&c.run).bind(job).bind(owner).bind(wanted).execute(&mut **tx).await?;
        issue(tx,c,"missing_job_settlement",job,Some(owner),json!({"job_id":job,"provider_owner_id":wanted,"execution_state":r.try_get::<&str,_>("execution_state")?,"settlement_state":r.try_get::<&str,_>("settlement_state")?})).await?;
        return Ok(());
    };
    let charge: i64 = r.try_get("settled_charge")?;
    let provider: i64 = r.try_get("provider_pending")?;
    let fee: i64 = r.try_get("platform_fee")?;
    let values = sqlx::query("SELECT account,amount FROM ledger_entries WHERE transaction_id=$1")
        .bind(transaction)
        .fetch_all(&mut **tx)
        .await?;
    let entries = values
        .iter()
        .map(|e| {
            Ok((
                e.try_get::<&str, _>("account")?,
                e.try_get::<i64, _>("amount")?,
            ))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, sqlx::Error>>()?;
    let terms_match: bool = r.try_get("terms_match")?;
    let rate = r.try_get::<Option<i32>, _>("fee_bps")?;
    let expected_split = rate.and_then(|v| u16::try_from(v).ok()).and_then(|rate| {
        orange_domain::Credits::new(charge)
            .ok()
            .and_then(|amount| orange_domain::split_fee(amount, rate).ok())
    });
    let buckets = [
        r.try_get("held_gift")?,
        r.try_get("held_earned")?,
        r.try_get("held_purchased")?,
    ];
    let mut valid = terms_match
        && expected_split
            .is_some_and(|(share, income)| share.units() == fee && income.units() == provider)
        && buckets.iter().map(|v| i128::from(*v)).sum::<i128>()
            == i128::from(r.try_get::<i64, _>("reserved")?)
        && correct_postings(&entries, owner, wanted, buckets, charge, fee, provider)
        && r.try_get::<&str, _>("execution_state")? == "succeeded"
        && r.try_get::<&str, _>("settlement_state")? == "settled"
        && r.try_get::<Option<i64>, _>("job_charge")? == Some(charge)
        && r.try_get::<&str, _>("provider_owner_id")? == wanted
        && r.try_get::<&str, _>("ledger_owner")? == owner
        && r.try_get::<&str, _>("business_key")? == format!("settle:{job}");
    let purchased = r.try_get::<i64, _>("held_purchased")?;
    let returned = entries
        .get(format!("{owner}:purchased:available").as_str())
        .copied()
        .unwrap_or(0);
    // Legacy purchased credit outside tracked lots is spent before FIFO tracked purchases.
    let lots_ok: bool = sqlx::query_scalar("SELECT coalesce(bool_and(l.owner_id=$2 AND a.charged IS NOT NULL),false)
        AND sum(a.held)<=$3::bigint AND sum(a.charged)=greatest(0::numeric,sum(a.held)-$4::bigint)
        FROM test_job_lot_allocations a JOIN test_purchase_lots l USING(order_id) WHERE a.job_id=$1")
        .bind(job).bind(owner).bind(purchased).bind(returned).fetch_one(&mut **tx).await?;
    valid &= lots_ok;
    sqlx::query("INSERT INTO test_statement_job_checks(run_id,job_id,owner_id,expected_provider,transaction_id,charge,verified) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(&c.run).bind(job).bind(owner).bind(wanted).bind(transaction).bind(charge).bind(valid).execute(&mut **tx).await?;
    if !valid {
        issue(tx,c,"job_posting_mismatch",job,Some(owner),json!({"job_id":job,"transaction_id":transaction,"provider_owner_id":wanted,"charge":charge.to_string(),"provider_pending":provider.to_string(),"platform_fee":fee.to_string()})).await?;
    }
    Ok(())
}
async fn seal(tx: &mut Tx<'_>, c: &Claim) -> Result<(), Error> {
    let period = c.until - DAY;

    let cash:Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY currency,category,provider_status),'[]') FROM (
        SELECT currency,category,provider_status,count(*)::text AS lines,sum(gross)::text AS gross_minor,sum(fee)::text AS fee_minor,sum(net)::text AS net_minor
        FROM test_statement_run_lines WHERE run_id=$1 AND created_unix>=$2 GROUP BY currency,category,provider_status) x")
        .bind(&c.run).bind(period).fetch_one(&mut **tx).await?;
    let original:Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(x) ORDER BY currency,flow),'[]') FROM (
        SELECT currency,flow,count(*)::text AS entries,sum(gross)::text AS original_minor,sum(units)::text AS purchased_bucket_change
        FROM test_statement_run_obligations WHERE run_id=$1 AND recorded_at>=to_timestamp($2::bigint) AND recorded_at<to_timestamp($3::bigint) GROUP BY currency,flow) x")
        .bind(&c.run).bind(period).bind(c.until).fetch_one(&mut **tx).await?;
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT issue_key) FROM test_statement_issue_evidence WHERE run_id=$1",
    )
    .bind(&c.run)
    .fetch_one(&mut **tx)
    .await?;
    let jobs:Value=sqlx::query_scalar("SELECT jsonb_build_object('settled_jobs',count(*)::text,'charged_microcredits',coalesce(sum(s.charge),0)::text,
        'provider_pending_microcredits',coalesce(sum(s.provider_pending),0)::text,'platform_fee_microcredits',coalesce(sum(s.platform_fee),0)::text,
        'promotional_microcredits_used',coalesce(sum(-coalesce((SELECT sum(e.amount) FROM ledger_entries e WHERE e.transaction_id=s.transaction_id AND
          e.account IN(j.owner_id||':gift:reserved',j.owner_id||':gift:available')),0)),0)::text)
        FROM job_settlements s JOIN jobs j USING(job_id) WHERE s.settled_at>=to_timestamp($1::bigint) AND s.settled_at<to_timestamp($2::bigint)")
        .bind(period).bind(c.until).fetch_one(&mut **tx).await?;
    let resources:Value=sqlx::query_scalar("SELECT jsonb_build_object('attempts',count(*)::text,'additional_attempts',count(*) FILTER (WHERE EXISTS(SELECT 1 FROM worker_execution_attempts p WHERE p.job_id=a.job_id AND (p.created_at,p.attempt_id)<(a.created_at,a.attempt_id)))::text)
        FROM worker_execution_attempts a WHERE created_at>=to_timestamp($1::bigint) AND created_at<to_timestamp($2::bigint)")
        .bind(period).bind(c.until).fetch_one(&mut **tx).await?;
    let storage:String=sqlx::query_scalar("SELECT floor(coalesce(sum(size_bytes::numeric*extract(epoch FROM least(coalesce(deleted_at,to_timestamp($2::bigint)),to_timestamp($2::bigint))-greatest(published_at,to_timestamp($1::bigint)))),0))::text
        FROM artifacts WHERE published_at<to_timestamp($2::bigint) AND (deleted_at IS NULL OR deleted_at>to_timestamp($1::bigint))")
        .bind(period).bind(c.until).fetch_one(&mut **tx).await?;
    let report = json!({"scan_from_unix":c.from.to_string(),"from_unix":period.to_string(),"until_unix":c.until.to_string(),
        "merchant_statement_by_settlement_currency":cash,"local_business_by_original_currency":original,
        "platform_wide_job_credits":jobs,"platform_wide_execution":resources,"published_artifact_byte_seconds":storage,"metered_storage_cash_cost":Value::Null,"metered_bandwidth_cash_cost":Value::Null,
        "observed_scan_issues":issues.to_string()});
    sqlx::query("UPDATE test_statement_runs SET completed_at=clock_timestamp(),report=$2 WHERE run_id=$1 AND completed_at IS NULL").bind(&c.run).bind(report).execute(&mut **tx).await?;
    sqlx::query("UPDATE test_statement_sync SET high_water=$2,run_id=NULL,phase='external',page_cursor='',claim_until=NULL,failures=0,last_error=NULL,next_check=CASE WHEN $2<floor(extract(epoch FROM clock_timestamp())/86400)::bigint*86400 THEN clock_timestamp() ELSE to_timestamp($2::bigint+86400) END WHERE merchant_id=$1")
        .bind(&c.merchant).bind(c.until).execute(&mut **tx).await?;
    Ok(())
}
async fn step(pool: &PgPool, provider: &Sandbox, c: &Claim) -> Result<(), Error> {
    let page = if c.phase == "external" {
        Some(provider.balance_page(c.from, c.until, &c.cursor).await?)
    } else {
        None
    };
    let mut tx = pool.begin().await?;
    let live=sqlx::query("SELECT merchant_id FROM test_statement_sync WHERE merchant_id=$1 AND run_id=$2 AND generation=$3 AND claim_until>clock_timestamp() FOR UPDATE")
        .bind(&c.merchant).bind(&c.run).bind(c.generation).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Ok(());
    }
    let (phase, cursor) = if let Some((data, more)) = page {
        for line in &data {
            ingest(&mut tx, c, line).await?;
        }
        if more {
            ("external", data.last().ok_or(Error::Mismatch)?.id.clone())
        } else {
            ("local", String::new())
        }
    } else if c.phase == "local" {
        let rows=sqlx::query("SELECT *,flow||':'||business_id AS cursor FROM test_statement_obligations WHERE merchant_id=$1 AND recorded_at>=to_timestamp($2::bigint) AND recorded_at<to_timestamp($3::bigint) AND flow||':'||business_id>$4 ORDER BY flow||':'||business_id LIMIT 100")
            .bind(&c.merchant).bind(c.from).bind(c.until).bind(&c.cursor).fetch_all(&mut *tx).await?;
        for ob in &rows {
            sqlx::query("INSERT INTO test_statement_run_obligations(run_id,flow,business_id,owner_id,source_id,currency,gross,units,transaction_id,recorded_at)
               SELECT $1,flow,business_id,owner_id,source_id,currency,gross,units,transaction_id,recorded_at FROM test_statement_obligations WHERE merchant_id=$2 AND flow=$3 AND business_id=$4")
                .bind(&c.run).bind(&c.merchant).bind(ob.try_get::<&str,_>("flow")?).bind(ob.try_get::<&str,_>("business_id")?).execute(&mut *tx).await?;

            compare(&mut tx, c, ob).await?;
        }
        if rows.len() < 100 {
            ("jobs", String::new())
        } else {
            (
                "local",
                rows.last().ok_or(Error::Mismatch)?.try_get("cursor")?,
            )
        }
    } else if c.phase == "jobs" {
        let rows=sqlx::query("SELECT j.job_id,j.owner_id,j.execution_state,j.settlement_state,j.charge AS job_charge,j.reserved,j.held_gift,j.held_earned,j.held_purchased,
              (q.owner_id=j.owner_id AND q.root_budget_id=j.root_budget_id AND q.invocation_id=j.invocation_id AND q.max_amount=j.reserved AND EXISTS
                 (SELECT 1 FROM budget_roots b WHERE b.root_budget_id=j.root_budget_id AND b.owner_id=j.owner_id)) AS terms_match,
              CASE WHEN d.modality='transcription' THEN (SELECT fee_bps FROM transcription_quote_terms WHERE quote_id=q.quote_id AND price_version=q.price_version AND owner_id=j.owner_id)
                   WHEN d.modality='text' THEN (SELECT fee_bps FROM quote_metering WHERE quote_id=q.quote_id AND price_version=q.price_version AND owner_id=j.owner_id) END AS fee_bps,
              d.provider_owner_id AS expected_provider,s.transaction_id,s.charge AS settled_charge,s.provider_pending,s.platform_fee,s.provider_owner_id,l.owner_id AS ledger_owner,l.business_key
           FROM current_metered_deliveries d JOIN jobs j USING(job_id) JOIN quotes q ON q.quote_id=j.quote_id LEFT JOIN job_settlements s USING(job_id) LEFT JOIN ledger_transactions l USING(transaction_id)
           WHERE d.committed_at>=to_timestamp($2::bigint) AND d.committed_at<to_timestamp($3::bigint) AND j.job_id>$4
             AND EXISTS(SELECT 1 FROM test_job_lot_allocations a JOIN test_checkout_orders o USING(order_id) WHERE a.job_id=j.job_id AND o.merchant_id=$1)
           ORDER BY j.job_id LIMIT 100")
            .bind(&c.merchant).bind(c.from).bind(c.until).bind(&c.cursor).fetch_all(&mut *tx).await?;
        for job in &rows {
            check_job(&mut tx, c, job).await?;
        }
        if rows.len() < 100 {
            ("holds", String::new())
        } else {
            (
                "jobs",
                rows.last().ok_or(Error::Mismatch)?.try_get("job_id")?,
            )
        }
    } else {
        // Review a hold when it reaches one day of age. The overlap also catches records
        // committed concurrently with the preceding run. Retain the original reserve.
        let rows=sqlx::query("SELECT *,kind||':'||business_id AS cursor FROM test_statement_pending_items
          WHERE merchant_id=$1 AND opened_at>=to_timestamp($2::bigint-86400) AND opened_at<to_timestamp($3::bigint-86400)
            AND kind||':'||business_id>$4 ORDER BY kind||':'||business_id LIMIT 100")
            .bind(&c.merchant).bind(c.from).bind(c.until).bind(&c.cursor).fetch_all(&mut *tx).await?;
        for r in &rows {
            issue(&mut tx,c,r.try_get("kind")?,r.try_get("business_id")?,Some(r.try_get("owner_id")?),
                  json!({"state":r.try_get::<&str,_>("state")?,"held_microcredits":r.try_get::<Option<i64>,_>("held_units")?.map(|n|n.to_string()),"review_age_seconds":"86400"})).await?;
        }
        if rows.len() < 100 {
            seal(&mut tx, c).await?;
            tx.commit().await?;
            return Ok(());
        }
        (
            "holds",
            rows.last().ok_or(Error::Mismatch)?.try_get("cursor")?,
        )
    };
    sqlx::query("UPDATE test_statement_sync SET phase=$2,page_cursor=$3,claim_until=NULL,next_check=clock_timestamp(),failures=0,last_error=NULL WHERE merchant_id=$1")
        .bind(&c.merchant).bind(phase).bind(cursor).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
impl Payments {
    pub async fn scan_statement_page(&self) -> Result<bool, Error> {
        let pool = &self.identity.pool;
        let Some(c) = claim(pool, &self.provider.merchant).await? else {
            return Ok(false);
        };
        if let Err(e) = step(pool, &self.provider, &c).await {
            let reason = if matches!(e, Error::Mismatch) {
                "evidence_mismatch"
            } else {
                "scan_unavailable"
            };
            let mut tx = pool.begin().await?;
            let changed=sqlx::query("UPDATE test_statement_sync SET claim_until=NULL,failures=least(failures+1,30),last_error=$3,next_check=clock_timestamp()+make_interval(secs=>least(3600,30*(1<<least(failures,7)))) WHERE merchant_id=$1 AND generation=$2 AND run_id=$4")
                .bind(&c.merchant).bind(c.generation).bind(reason).bind(&c.run).execute(&mut *tx).await?.rows_affected();
            if changed == 1 {
                sqlx::query("INSERT INTO test_statement_failures(run_id,generation,phase,page_cursor,reason) VALUES($1,$2,$3,$4,$5)").bind(&c.run).bind(c.generation).bind(&c.phase).bind(&c.cursor).bind(reason).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Err(e);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::correct_postings;
    use std::collections::BTreeMap;

    #[test]
    fn balanced_entries_still_require_the_original_buckets_and_supplier() {
        let mut entries = BTreeMap::from([
            ("team:gift:reserved", -10),
            ("team:gift:available", 0),
            ("team:earned:reserved", -10),
            ("team:earned:available", 0),
            ("team:purchased:reserved", -30),
            ("team:purchased:available", 20),
            ("alice:earned:pending", 24),
            ("platform:fees", 6),
        ]);
        let valid =
            |e: &BTreeMap<&str, i64>| correct_postings(e, "team", "alice", [10, 10, 30], 30, 6, 24);
        assert!(valid(&entries));
        entries.insert("team:gift:available", 2);
        entries.insert("team:purchased:available", 18);
        assert!(
            !valid(&entries),
            "balanced amounts refund the wrong buckets"
        );
        entries.insert("team:gift:available", 0);
        entries.insert("team:purchased:available", 20);
        let payment = entries.remove("alice:earned:pending").unwrap();
        entries.insert("bob:earned:pending", payment);
        assert!(
            !valid(&entries),
            "same charge credited to a different supplier"
        );
    }
}
