//! Serializable, original-user withdrawal before admission. Existing Jobs use
//! the already verified cancellation path; no extra accepted command is lost
//! between transactions and no cleanup is inferred from this barrier.
use super::*;
use crate::artifacts::ArtifactActor;
use orange_protocol::artifacts::InvocationWithdrawal;
async fn invocation_lock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    invocation: &str,
) -> Result<(), StoreError> {
    let key = request_digest("user.invocation.lock.v1", &(owner, invocation))
        .map_err(|_| StoreError::Invalid)?;
    sqlx::query("SET LOCAL lock_timeout='3s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub(super) async fn submission_lock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    invocation: &str,
) -> Result<(), StoreError> {
    invocation_lock(tx, owner, invocation).await
}
pub(super) async fn check_not_withdrawn(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    actor: Option<&ArtifactActor>,
    invocation: &str,
) -> Result<(), StoreError> {
    if let Some(actor) = actor {
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM invocation_withdrawals WHERE owner_id=$1 AND user_id=$2 AND invocation_id=$3)").bind(owner).bind(&actor.user).bind(invocation).fetch_one(&mut **tx).await?;
        if blocked {
            return Err(StoreError::InvocationWithdrawn);
        }
    }
    Ok(())
}
enum Decision {
    Prevented(String),
    Existing(String),
}
impl Store {
    pub async fn withdraw_invocation(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        key: &str,
        req: &SubmitJob,
    ) -> Result<InvocationWithdrawal, StoreError> {
        if !matches!(
            req.purpose,
            orange_protocol::InvocationPurpose::Primary
                | orange_protocol::InvocationPurpose::Summary
                | orange_protocol::InvocationPurpose::Memory
                | orange_protocol::InvocationPurpose::Title
                | orange_protocol::InvocationPurpose::Continuation
        ) {
            return Err(StoreError::Invalid);
        }
        if !actor.write
            || ![
                owner,
                key,
                &req.root_budget_id,
                &req.invocation_id,
                &req.quote_id,
            ]
            .iter()
            .all(|v| valid_id(v))
            || !valid_digest(&req.request_projection_digest)
        {
            return Err(StoreError::Invalid);
        }
        let mut decision = None;
        for attempt in 0..3 {
            match self.withdraw_once(actor, owner, key, req).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => {
                    decision = Some(result?);
                    break;
                }
            }
        }
        let (receipt_id, job) = match decision.ok_or(StoreError::Conflict)? {
            Decision::Prevented(receipt) => (Some(receipt), None),
            // No cancellation decision was committed in the read transaction.
            // Only this existing cancel_for_user transaction accepts the stop.
            Decision::Existing(job) => (
                None,
                Some(
                    self.cancel_for_user(
                        actor,
                        owner,
                        &job,
                        &request_digest(
                            "withdraw.existing.v1",
                            &(owner, &actor.user, &req.invocation_id, key),
                        )
                        .map_err(|_| StoreError::Invalid)?,
                    )
                    .await?,
                ),
            ),
        };
        Ok(InvocationWithdrawal {
            owner_id: owner.into(),
            user_id: actor.user.clone(),
            invocation_id: req.invocation_id.clone(),
            quote_id: req.quote_id.clone(),
            prevented: job.is_none(),
            receipt_id,
            job,
        })
    }
    async fn withdraw_once(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        key: &str,
        req: &SubmitJob,
    ) -> Result<Decision, StoreError> {
        let digest = request_digest("submit_job", req).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        crate::artifacts::authority::scope(&mut tx, actor, owner, false)
            .await
            .map_err(artifact_error)?;
        authorize(&mut tx, owner).await?;
        invocation_lock(&mut tx, owner, &req.invocation_id).await?;
        // Both this and submission remain SERIALIZABLE: a transaction whose
        // snapshot predates a waited-on barrier must retry instead of missing it.
        if let Some(row) =
            sqlx::query("SELECT * FROM jobs WHERE owner_id=$1 AND invocation_id=$2 FOR SHARE")
                .bind(owner)
                .bind(&req.invocation_id)
                .fetch_optional(&mut *tx)
                .await?
        {
            if row
                .try_get::<Option<String>, _>("submitting_user_id")?
                .as_deref()
                != Some(actor.user.as_str())
            {
                return Err(StoreError::Forbidden);
            }
            if row.try_get::<String, _>("request_digest")? != digest {
                return Err(StoreError::Conflict);
            }
            let job = row.try_get("job_id")?;
            finish_submit(tx, Some(actor)).await?;
            return Ok(Decision::Existing(job));
        }
        let prior=sqlx::query("SELECT * FROM invocation_withdrawals WHERE owner_id=$1 AND user_id=$2 AND invocation_id=$3").bind(owner).bind(&actor.user).bind(&req.invocation_id).fetch_all(&mut *tx).await?;
        if let Some(first) = prior.first() {
            if prior.len() != 1
                || first.try_get::<String, _>("request_digest")? != digest
                || first.try_get::<String, _>("invocation_id")? != req.invocation_id
            {
                return Err(StoreError::Conflict);
            }
            let receipt = first.try_get("receipt_id")?;
            finish_submit(tx, Some(actor)).await?;
            return Ok(Decision::Prevented(receipt));
        }
        let quote=sqlx::query("SELECT q.*,m.requester_id FROM quotes q LEFT JOIN quote_metering m USING(quote_id) JOIN budget_roots b ON b.root_budget_id=q.root_budget_id AND b.owner_id=q.owner_id WHERE q.quote_id=$1 AND q.owner_id=$2 FOR SHARE OF q,b").bind(&req.quote_id).bind(owner).fetch_optional(&mut *tx).await?.ok_or(StoreError::NotFound)?;
        if quote.try_get::<String, _>("root_budget_id")? != req.root_budget_id
            || quote.try_get::<String, _>("invocation_id")? != req.invocation_id
            || quote.try_get::<String, _>("projection_digest")? != req.request_projection_digest
        {
            return Err(StoreError::QuoteMismatch);
        }
        let requester: Option<String> = quote.try_get("requester_id")?;
        let personal:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE user_id=$1 AND personal_owner_id=$2 AND active)").bind(&actor.user).bind(owner).fetch_one(&mut *tx).await?;
        if requester.as_deref().is_some_and(|user| user != actor.user)
            || (!personal && requester.as_deref() != Some(actor.user.as_str()))
        {
            return Err(StoreError::Forbidden);
        }
        let receipt = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO invocation_withdrawals(owner_id,user_id,invocation_id,root_budget_id,quote_id,request_key,request_digest,receipt_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(owner).bind(&actor.user).bind(&req.invocation_id).bind(&req.root_budget_id).bind(&req.quote_id).bind(key).bind(&digest).bind(&receipt).execute(&mut *tx).await?;
        emit(
            &mut tx,
            owner,
            &req.invocation_id,
            "invocation.withdrawn_before_acceptance",
            &digest,
        )
        .await?;
        finish_submit(tx, Some(actor)).await?;
        Ok(Decision::Prevented(receipt))
    }
}
