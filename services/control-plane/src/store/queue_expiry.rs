//! Trusted queue expiry and shared zero-charge release. Never infers device
//! cleanup, revives a stopped Job or refunds an accepted execution attempt.
use super::*;
use sqlx::postgres::PgRow;
/// Service-owned scan position, not financial authority. Restarting a scan is safe:
/// every candidate is revalidated transactionally. Advance past failures so a
/// persistent integrity fault cannot starve the rest of the queue.
#[derive(Default)]
pub struct QueueExpiryCursor {
    after_job: Option<String>,
}
pub(super) enum QueueEnd {
    Cancelled,
    Expired,
}
pub(super) async fn release_queue(
    tx: &mut Transaction<'_, Postgres>,
    root: &PgRow,
    row: &PgRow,
    end: QueueEnd,
) -> Result<String, StoreError> {
    let current = job_view(row)?;
    if current.execution_state != "queued" || current.settlement_state != "pending" {
        return Err(StoreError::InvalidTransition);
    }
    let owner: String = row.try_get("owner_id")?;
    let job = &current.job_id;
    let protected:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM job_response_commits WHERE job_id=$1) OR EXISTS(SELECT 1 FROM job_usage_facts WHERE job_id=$1) OR EXISTS(SELECT 1 FROM worker_execution_attempts WHERE job_id=$1 AND (start_budget_ms IS NOT NULL OR broker_grant_id IS NOT NULL OR state IN('started','closed'))) OR EXISTS(SELECT 1 FROM worker_execution_events e JOIN worker_execution_attempts a USING(attempt_id) WHERE a.job_id=$1 AND e.kind='start.granted')").bind(job).fetch_one(&mut **tx).await?;
    if protected {
        return Err(StoreError::Conflict);
    }
    let held: [i64; 3] = [
        row.try_get("held_gift")?,
        row.try_get("held_earned")?,
        row.try_get("held_purchased")?,
    ];
    if held.iter().any(|amount| *amount < 0)
        || held.iter().map(|amount| i128::from(*amount)).sum::<i128>()
            != i128::from(current.reserved.0.units())
    {
        return Err(StoreError::Invalid);
    }
    let next = budget_model(root)?
        .settle(current.reserved.0, Credits::ZERO)
        .map_err(|_| StoreError::Budget)?;
    sqlx::query("UPDATE wallets SET gift=gift+$2,earned=earned+$3,purchased=purchased+$4,reserved_gift=reserved_gift-$2,reserved_earned=reserved_earned-$3,reserved_purchased=reserved_purchased-$4 WHERE owner_id=$1").bind(&owner).bind(held[0]).bind(held[1]).bind(held[2]).execute(&mut **tx).await?;
    sqlx::query("UPDATE budget_roots SET reserved=$2 WHERE root_budget_id=$1")
        .bind(&current.root_budget_id)
        .bind(next.reserved.units())
        .execute(&mut **tx)
        .await?;
    let mut entries = Vec::new();
    for (bucket, amount) in ["gift", "earned", "purchased"].iter().zip(held) {
        entries.push((format!("{owner}:{bucket}:reserved"), -amount));
        entries.push((format!("{owner}:{bucket}:available"), amount));
    }
    let key = format!("release:{job}");
    ledger(tx, &owner, &key, entries).await?;
    let transaction: String =
        sqlx::query_scalar("SELECT transaction_id FROM ledger_transactions WHERE business_key=$1")
            .bind(&key)
            .fetch_one(&mut **tx)
            .await?;
    let (state, event, reason) = match end {
        QueueEnd::Cancelled => ("cancelled", "job.cancelled", "zero_charge_queue_cancel"),
        QueueEnd::Expired => ("failed", "job.queue_expired", "deadline_before_start"),
    };
    sqlx::query(
        "UPDATE jobs SET execution_state=$2,settlement_state='settled',charge=0 WHERE job_id=$1",
    )
    .bind(job)
    .bind(state)
    .execute(&mut **tx)
    .await?;
    // Financial finalization changes no attempt state and makes no local resource claim.
    sqlx::query("DELETE FROM artifact_pins WHERE reference_id=$1 AND artifact_id IN(SELECT artifact_id FROM job_inputs WHERE job_id=$2)").bind(format!("job-input:{job}")).bind(job).execute(&mut **tx).await?;
    emit(tx, &owner, job, event, reason).await?;
    Ok(transaction)
}
impl Store {
    /// Service maintenance only. No HTTP authority or user-controlled deadline.
    pub async fn expire_queued_ready(
        &self,
        cursor: &mut QueueExpiryCursor,
        limit: u16,
    ) -> Result<(u32, u32), StoreError> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::Invalid);
        }
        let jobs:Vec<String>=sqlx::query_scalar("SELECT j.job_id FROM jobs j JOIN quote_execution_plans p USING(quote_id) WHERE j.execution_state='queued' AND j.settlement_state='pending' AND p.job_deadline<=clock_timestamp() AND ($2::text IS NULL OR j.job_id>$2) ORDER BY j.job_id LIMIT $1").bind(i64::from(limit)).bind(&cursor.after_job).fetch_all(&self.pool).await?;
        if jobs.is_empty() {
            // Retry earlier failures and newly due jobs on the following tick.
            cursor.after_job = None;
        }
        let (mut expired, mut failed) = (0, 0);
        for job in jobs {
            match self.expire_queued(&job).await {
                Ok(true) => expired += 1,
                Ok(false) => {}
                Err(_) => failed += 1,
            }
            cursor.after_job = Some(job);
        }
        Ok((expired, failed))
    }
    pub async fn expire_queued(&self, job: &str) -> Result<bool, StoreError> {
        if !valid_id(job) {
            return Err(StoreError::Invalid);
        }
        for attempt in 0..3 {
            match self.expire_once(job).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => return result,
            }
        }
        unreachable!("bounded retry returns")
    }
    async fn expire_once(&self, job: &str) -> Result<bool, StoreError> {
        let mut tx = self.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        let identity = sqlx::query("SELECT owner_id,root_budget_id FROM jobs WHERE job_id=$1")
            .bind(job)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let owner: String = identity.try_get("owner_id")?;
        let root_id: String = identity.try_get("root_budget_id")?;
        // Current user restrictions do not confiscate previously held credits.
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE owner_id=$1 AND root_budget_id=$2 FOR UPDATE",
        )
        .bind(&owner)
        .bind(&root_id)
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
        if row.try_get::<String, _>("execution_state")? != "queued"
            || row.try_get::<String, _>("settlement_state")? != "pending"
        {
            tx.commit().await?;
            return Ok(false);
        }
        let quote: String = row.try_get("quote_id")?;
        let plan=sqlx::query("SELECT p.job_deadline<=clock_timestamp() AS expired,q.owner_id,q.root_budget_id,q.invocation_id,q.max_amount,p.request_projection_digest=q.projection_digest AS projection_matches FROM quote_execution_plans p JOIN quotes q USING(quote_id) WHERE p.quote_id=$1").bind(&quote).fetch_optional(&mut *tx).await?;
        let Some(plan) = plan else {
            tx.commit().await?;
            return Ok(false);
        };
        if !plan.try_get::<bool, _>("expired")? {
            tx.commit().await?;
            return Ok(false);
        }
        if plan.try_get::<String, _>("owner_id")? != owner
            || plan.try_get::<String, _>("root_budget_id")? != root_id
            || plan.try_get::<String, _>("invocation_id")?
                != row.try_get::<String, _>("invocation_id")?
            || plan.try_get::<i64, _>("max_amount")? != row.try_get::<i64, _>("reserved")?
            || !plan.try_get::<bool, _>("projection_matches")?
        {
            return Err(StoreError::Conflict);
        }
        let transaction = release_queue(&mut tx, &root, &row, QueueEnd::Expired).await?;
        sqlx::query("INSERT INTO job_queue_expirations(job_id,quote_id,job_deadline,transaction_id) SELECT $1,quote_id,job_deadline,$2 FROM quote_execution_plans WHERE quote_id=$3").bind(job).bind(transaction).bind(quote).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
}
