//! Drive settlement only from durable server measurements. Financial changes stay in the original settlement transaction.
use super::*;
use sqlx::Acquire;

async fn validate(tx: &mut Tx<'_>, job: &str) -> Result<bool, ArtifactError> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM metering_receipts r JOIN current_metered_deliveries d USING(job_id)
        JOIN jobs j USING(job_id) JOIN quotes q ON q.quote_id=j.quote_id JOIN job_response_commits c ON c.job_id=j.job_id
        WHERE j.job_id=$1 AND r.event_id=c.commit_id AND c.response_digest=r.response_digest AND j.owner_id=r.owner_id
          AND q.owner_id=r.owner_id AND q.root_budget_id=j.root_budget_id AND q.invocation_id=j.invocation_id AND q.max_amount=j.reserved
          AND ((d.modality='text' AND EXISTS(SELECT 1 FROM job_usage_facts u JOIN quote_metering m ON m.quote_id=j.quote_id
             WHERE u.job_id=j.job_id AND u.response_digest=c.response_digest AND m.owner_id=r.owner_id AND u.input_units=m.input_units
             AND m.meter_profile_digest=u.meter_profile_digest AND u.request_projection_digest=q.projection_digest))
          OR (d.modality='transcription' AND EXISTS(SELECT 1 FROM transcription_response_facts f JOIN transcription_quote_terms t ON t.quote_id=j.quote_id
             WHERE f.job_id=j.job_id AND f.response_digest=c.response_digest AND f.owner_id=r.owner_id AND f.grant_id=t.grant_id AND f.duration_ms=t.duration_ms))))")
        .bind(job).fetch_one(&mut **tx).await?;
    if valid {
        sqlx::query("WITH v AS (UPDATE metering_receipts SET state='validated' WHERE job_id=$1 AND state='received'
           RETURNING event_id,generation,attempts) INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason)
           SELECT event_id,generation,attempts,'validated','matched_quote_and_usage' FROM v")
           .bind(job).execute(&mut **tx).await?;
    }
    Ok(valid)
}

async fn fail(
    tx: &mut Tx<'_>,
    job: &str,
    reason: &str,
    permanent: bool,
) -> Result<(), ArtifactError> {
    let r=sqlx::query("SELECT r.event_id,r.owner_id,r.attempts,r.generation,r.deadline<=clock_timestamp() AS expired,
        p.max_attempts,p.base_delay_seconds,p.max_delay_seconds FROM metering_receipts r JOIN metering_retry_policies p USING(policy_id) WHERE r.job_id=$1")
        .bind(job).fetch_one(&mut **tx).await?;
    let attempts: i32 = r.try_get("attempts")?;
    let expired: bool = r.try_get("expired")?;
    let dead = expired || permanent || attempts >= r.try_get::<i32, _>("max_attempts")?;
    let reason = if expired { "retry_deadline" } else { reason };
    let stage = if dead {
        let case = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at)
            VALUES($1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint)")
            .bind(&case).bind(r.try_get::<&str,_>("owner_id")?).bind(format!("metering:{}",r.try_get::<&str,_>("event_id")?)).execute(&mut **tx).await?;
        sqlx::query("UPDATE metering_receipts SET state='dead',case_id=$2 WHERE job_id=$1")
            .bind(job)
            .bind(case)
            .execute(&mut **tx)
            .await?;
        "dead"
    } else {
        // A single immediate retry preserves legacy recovery after one interrupted transaction.
        let delay = if attempts <= 1 {
            0
        } else {
            r.try_get::<i32, _>("base_delay_seconds")?
                .saturating_mul(1i32 << (attempts - 2).min(20))
                .min(r.try_get("max_delay_seconds")?)
        };
        sqlx::query("UPDATE metering_receipts SET next_attempt=least(deadline,clock_timestamp()+make_interval(secs=>$2)) WHERE job_id=$1")
            .bind(job).bind(f64::from(delay)).execute(&mut **tx).await?;
        "retry"
    };
    sqlx::query("INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason) VALUES($1,$2,$3,$4,$5)")
        .bind(r.try_get::<&str,_>("event_id")?).bind(r.try_get::<i64,_>("generation")?).bind(attempts).bind(stage).bind(reason).execute(&mut **tx).await?;
    Ok(())
}

impl ArtifactStore {
    pub(crate) async fn settle_committed(&self, job: &str) -> Result<bool, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        let r=sqlx::query("SELECT state,deadline<=clock_timestamp() AS expired,next_attempt<=clock_timestamp() AS due FROM metering_receipts WHERE job_id=$1 FOR UPDATE")
            .bind(job).fetch_optional(&mut *tx).await?;
        let Some(r) = r else { return Ok(false) };
        match r.try_get::<&str, _>("state")? {
            "posted" => return super::settlement::post_measured(&mut tx, job).await,
            "dead" => return Err(ArtifactError::PricingUnavailable),
            _ => {}
        }
        if r.try_get::<bool, _>("expired")? {
            fail(&mut tx, job, "retry_deadline", true).await?;
            tx.commit().await?;
            return Err(ArtifactError::Expired);
        }
        if !r.try_get::<bool, _>("due")? {
            return Err(ArtifactError::NotReady);
        }
        sqlx::query("UPDATE metering_receipts SET attempts=attempts+1 WHERE job_id=$1")
            .bind(job)
            .execute(&mut *tx)
            .await?;
        if !validate(&mut tx, job).await? {
            fail(&mut tx, job, "invalid_metering_evidence", true).await?;
            tx.commit().await?;
            return Err(ArtifactError::Integrity);
        }
        let mut posting = tx.begin().await?;
        match super::settlement::post_measured(&mut posting, job).await {
            Ok(true) => {
                posting.commit().await?;
                tx.commit().await?;
                Ok(true)
            }
            outcome => {
                posting.rollback().await?;
                let error = outcome.err().unwrap_or(ArtifactError::Integrity);
                let permanent = matches!(
                    error,
                    ArtifactError::Invalid
                        | ArtifactError::Forbidden
                        | ArtifactError::Conflict
                        | ArtifactError::Integrity
                        | ArtifactError::Expired
                        | ArtifactError::Deleted
                );
                fail(
                    &mut tx,
                    job,
                    if permanent {
                        "invalid_metering_evidence"
                    } else {
                        "settlement_unavailable"
                    },
                    permanent,
                )
                .await?;
                tx.commit().await?;
                Err(error)
            }
        }
    }
}
