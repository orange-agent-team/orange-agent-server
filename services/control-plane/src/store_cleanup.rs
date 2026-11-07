//! System-only zero-charge release after exact cloud cleanup and no delivery.
use crate::store::{StoreError, allocate, budget_model, ledger};
use orange_domain::Credits;
use sqlx::{PgPool, Row};
pub(crate) async fn finalize(pool: &PgPool, job: &str) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='3s'")
        .execute(&mut *tx)
        .await?;
    let identity=sqlx::query("SELECT j.owner_id,j.root_budget_id,c.receipt_id FROM jobs j JOIN worker_cpu_cleanup_receipts c USING(job_id) WHERE j.job_id=$1").bind(job).fetch_optional(&mut *tx).await?.ok_or(StoreError::NotFound)?;
    let owner: String = identity.try_get("owner_id")?;
    let root_id: String = identity.try_get("root_budget_id")?;
    let root = sqlx::query(
        "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
    )
    .bind(&root_id)
    .bind(&owner)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
        .bind(&owner)
        .fetch_one(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT * FROM jobs WHERE job_id=$1 FOR UPDATE")
        .bind(job)
        .fetch_one(&mut *tx)
        .await?;
    let response: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM job_response_commits WHERE job_id=$1)")
            .bind(job)
            .fetch_one(&mut *tx)
            .await?;
    let state: String = row.try_get("execution_state")?;
    if !response
        && matches!(state.as_str(), "failed" | "cancelled")
        && row.try_get::<String, _>("settlement_state")? == "pending"
    {
        let usage: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM job_usage_facts WHERE job_id=$1)")
                .bind(job)
                .fetch_one(&mut *tx)
                .await?;
        if usage {
            return Err(StoreError::Conflict);
        }
        let held: [i64; 3] = [
            row.try_get("held_gift")?,
            row.try_get("held_earned")?,
            row.try_get("held_purchased")?,
        ];
        let reserved = Credits::new(row.try_get("reserved")?).map_err(|_| StoreError::Invalid)?;
        allocate(reserved.units(), held)?;
        let next = budget_model(&root)?
            .settle(reserved, Credits::ZERO)
            .map_err(|_| StoreError::Budget)?;
        sqlx::query("UPDATE wallets SET gift=gift+$2,earned=earned+$3,purchased=purchased+$4,reserved_gift=reserved_gift-$2,reserved_earned=reserved_earned-$3,reserved_purchased=reserved_purchased-$4 WHERE owner_id=$1").bind(&owner).bind(held[0]).bind(held[1]).bind(held[2]).execute(&mut *tx).await?;
        sqlx::query("UPDATE budget_roots SET reserved=$2 WHERE root_budget_id=$1")
            .bind(&root_id)
            .bind(next.reserved.units())
            .execute(&mut *tx)
            .await?;
        let mut entries = vec![];
        for (bucket, amount) in ["gift", "earned", "purchased"].iter().zip(held) {
            entries.push((format!("{owner}:{bucket}:reserved"), -amount));
            entries.push((format!("{owner}:{bucket}:available"), amount));
        }
        let key = format!("release:{job}");
        ledger(&mut tx, &owner, &key, entries).await?;
        let transaction: String = sqlx::query_scalar(
            "SELECT transaction_id FROM ledger_transactions WHERE business_key=$1",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO job_cleanup_releases(job_id,cleanup_receipt_id,transaction_id,released) VALUES($1,$2,$3,$4)").bind(job).bind(identity.try_get::<String,_>("receipt_id")?).bind(&transaction).bind(reserved.units()).execute(&mut *tx).await?;
        sqlx::query("UPDATE jobs SET settlement_state='settled',charge=0 WHERE job_id=$1")
            .bind(job)
            .execute(&mut *tx)
            .await?;
        crate::events::append(
            &mut tx,
            &owner,
            job,
            "job.undelivered_released",
            &transaction,
        )
        .await?;
    }
    let settled:bool=sqlx::query_scalar("SELECT settlement_state='settled' AND execution_state IN('succeeded','failed','cancelled') FROM jobs WHERE job_id=$1").bind(job).fetch_one(&mut *tx).await?;
    if settled {
        sqlx::query("DELETE FROM artifact_pins WHERE (reference_id=$1 AND artifact_id IN(SELECT artifact_id FROM job_inputs WHERE job_id=$3)) OR (reference_id=$2 AND artifact_id IN(SELECT artifact_id FROM worker_output_uploads WHERE job_id=$3))").bind(format!("job-input:{job}")).bind(format!("job-output:{job}")).bind(job).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
