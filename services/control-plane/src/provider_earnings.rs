//! Maturity and audited release of existing provider development credits.
use crate::admin::AdminError;
use sqlx::{PgPool, Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

pub async fn prepare_reviews(pool: &PgPool, limit: u16) -> Result<u64, sqlx::Error> {
    if !(1..=100).contains(&limit) {
        return Err(sqlx::Error::Protocol(
            "review page size outside 1..100".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let rows=sqlx::query("SELECT e.job_id,e.provider_owner FROM provider_earning_origins e
        WHERE e.eligible_at<=provider_credit_now() AND NOT EXISTS(SELECT 1 FROM provider_earning_cases c WHERE c.job_id=e.job_id)
        ORDER BY e.eligible_at,e.job_id LIMIT $1 FOR UPDATE OF e SKIP LOCKED")
        .bind(i64::from(limit)).fetch_all(&mut *tx).await?;
    let mut count = 0;
    for r in rows {
        let job: &str = r.try_get("job_id")?;
        // The same row can be visible in a READ COMMITTED snapshot taken just before a concurrent batch committed.
        let case = uuid::Uuid::new_v4().to_string();
        let inserted=sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at)
           SELECT $1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint WHERE NOT EXISTS(SELECT 1 FROM provider_earning_cases WHERE job_id=$4)")
           .bind(&case).bind(r.try_get::<&str,_>("provider_owner")?).bind(format!("provider-earning:{job}")).bind(job).execute(&mut *tx).await?;
        if inserted.rows_affected() == 1 {
            sqlx::query("INSERT INTO provider_earning_cases(job_id,case_id) VALUES($1,$2)")
                .bind(job)
                .bind(case)
                .execute(&mut *tx)
                .await?;
            count += 1;
        }
    }
    tx.commit().await?;
    Ok(count)
}

pub(crate) async fn eligible(tx: &mut Tx<'_>, case: &str, job: &str) -> Result<(), AdminError> {
    let permitted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_earning_status e JOIN reconciliation_cases c ON c.case_id=e.case_id
        WHERE e.job_id=$1 AND e.case_id=$2 AND e.disposition='ready_for_review' AND c.owner_id=e.provider_owner
          AND c.environment='dev' AND c.business_ref='provider-earning:'||e.job_id)")
        .bind(job).bind(case).fetch_one(&mut **tx).await?;
    if !permitted {
        return Err(AdminError::Conflict);
    }
    Ok(())
}

pub(crate) async fn release(
    tx: &mut Tx<'_>,
    case: &str,
    job: &str,
    action: &str,
) -> Result<(), AdminError> {
    let origin=sqlx::query("SELECT e.buyer_owner,e.provider_owner,e.units FROM provider_earning_origins e JOIN provider_earning_cases c USING(job_id)
        WHERE c.case_id=$1 AND e.job_id=$2 FOR UPDATE OF e").bind(case).bind(job).fetch_optional(&mut **tx).await?.ok_or(AdminError::Conflict)?;
    let buyer: &str = origin.try_get("buyer_owner")?;
    let supplier: &str = origin.try_get("provider_owner")?;
    let units: i64 = origin.try_get("units")?;
    let owners = [buyer, supplier];
    // Order all shared policy locks consistently. The authoritative disposition is read after acquisition.
    sqlx::query("SELECT owner_id FROM owners WHERE owner_id=ANY($1) ORDER BY owner_id FOR SHARE")
        .bind(&owners[..])
        .fetch_all(&mut **tx)
        .await?;
    sqlx::query("SELECT organization_id FROM organizations WHERE organization_id=ANY($1) ORDER BY organization_id FOR SHARE").bind(&owners[..]).fetch_all(&mut **tx).await?;
    sqlx::query("SELECT owner_id FROM test_spend_policy_fences WHERE owner_id=$1 FOR SHARE")
        .bind(buyer)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("SELECT n.node_id FROM jobs j JOIN quote_execution_plans p USING(quote_id) JOIN nodes n ON n.node_id=p.target_node_id WHERE j.job_id=$1 FOR SHARE OF n")
        .bind(job).fetch_optional(&mut **tx).await?;
    sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
        .bind(supplier)
        .fetch_one(&mut **tx)
        .await?;
    eligible(tx, case, job).await?;
    let updated=sqlx::query("UPDATE wallets w SET pending_earned=pending_earned-$2,earned=earned+$2 WHERE w.owner_id=$1
        AND w.pending_earned>=$2 AND w.earned::numeric+$2::bigint<=9223372036854775807
        AND w.pending_earned=(SELECT sum(e.units) FROM provider_earning_origins e WHERE e.provider_owner=$1
             AND NOT EXISTS(SELECT 1 FROM provider_earning_releases r WHERE r.job_id=e.job_id))")
        .bind(supplier).bind(units).execute(&mut **tx).await?;
    if updated.rows_affected() != 1 {
        return Err(AdminError::Conflict);
    }
    let key = format!("provider-release:{job}");
    crate::store::ledger(
        tx,
        supplier,
        &key,
        vec![
            (format!("{supplier}:earned:pending"), -units),
            (format!("{supplier}:earned:available"), units),
        ],
    )
    .await
    .map_err(|_| AdminError::Unavailable)?;
    let recorded = sqlx::query(
        "INSERT INTO provider_earning_releases(job_id,transaction_id,action_id,case_id)
        SELECT $1,transaction_id,$2,$3 FROM ledger_transactions WHERE business_key=$4",
    )
    .bind(job)
    .bind(action)
    .bind(case)
    .bind(&key)
    .execute(&mut **tx)
    .await?;
    if recorded.rows_affected() != 1 {
        return Err(AdminError::Conflict);
    }
    crate::events::append(tx, supplier, job, "provider.earning_released", &key).await?;
    Ok(())
}
