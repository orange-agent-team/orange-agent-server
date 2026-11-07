//! Internal development-credit settlement from immutable quote/usage facts.
//! No caller amount, native usage, external payment or GPU cleanup is accepted.
use super::*;
use orange_domain::{Credits, TextDelivery, TextQuote};
impl ArtifactStore {
    pub(crate) async fn settle_committed(&self, job: &str) -> Result<bool, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        let identity=sqlx::query("SELECT j.owner_id,j.root_budget_id,m.provider_owner_id FROM jobs j JOIN quote_metering m USING(quote_id) JOIN job_usage_facts u USING(job_id) WHERE j.job_id=$1").bind(job).fetch_optional(&mut *tx).await?;
        let Some(identity) = identity else {
            return Ok(false);
        };
        let owner: String = identity.try_get("owner_id")?;
        let root_id: String = identity.try_get("root_budget_id")?;
        let provider: String = identity.try_get("provider_owner_id")?;
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(&root_id)
        .bind(&owner)
        .fetch_one(&mut *tx)
        .await?;
        let mut owners = vec![owner.clone(), provider.clone()];
        owners.sort();
        owners.dedup();
        let locked: Vec<String> = sqlx::query_scalar(
            "SELECT owner_id FROM wallets WHERE owner_id=ANY($1) ORDER BY owner_id FOR UPDATE",
        )
        .bind(&owners)
        .fetch_all(&mut *tx)
        .await?;
        if locked != owners {
            return Err(ArtifactError::NotReady);
        }
        let row = sqlx::query("SELECT * FROM jobs WHERE job_id=$1 FOR UPDATE")
            .bind(job)
            .fetch_one(&mut *tx)
            .await?;
        if row.try_get::<String, _>("owner_id")? != owner
            || row.try_get::<String, _>("root_budget_id")? != root_id
            || row.try_get::<String, _>("execution_state")? != "succeeded"
        {
            return Err(ArtifactError::Conflict);
        }
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT charge FROM job_settlements WHERE job_id=$1")
                .bind(job)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(charge) = existing {
            if row.try_get::<String, _>("settlement_state")? != "settled"
                || row.try_get::<Option<i64>, _>("charge")? != Some(charge)
            {
                return Err(ArtifactError::Integrity);
            }
            tx.commit().await?;
            return Ok(true);
        }
        if row.try_get::<String, _>("settlement_state")? != "pending" {
            return Err(ArtifactError::Conflict);
        }
        let quote=sqlx::query("SELECT q.*,m.price_version AS snapshot_price_version,m.input_units,m.output_limit,m.input_price,m.output_price,m.fee_bps,m.provider_owner_id,m.meter_profile_digest,m.owner_id AS snapshot_owner FROM quotes q JOIN quote_metering m USING(quote_id) WHERE q.quote_id=$1").bind(row.try_get::<String,_>("quote_id")?).fetch_one(&mut *tx).await?;
        let fact=sqlx::query("SELECT u.*,c.response_digest AS committed_digest FROM job_usage_facts u JOIN job_response_commits c USING(job_id) WHERE u.job_id=$1").bind(job).fetch_one(&mut *tx).await?;
        let held = row.try_get::<i64, _>("reserved")?;
        if quote.try_get::<String, _>("snapshot_price_version")?
            != quote.try_get::<String, _>("price_version")?
            || quote.try_get::<String, _>("owner_id")? != owner
            || quote.try_get::<String, _>("snapshot_owner")? != owner
            || quote.try_get::<String, _>("root_budget_id")? != root_id
            || quote.try_get::<String, _>("invocation_id")?
                != row.try_get::<String, _>("invocation_id")?
            || quote.try_get::<String, _>("provider_owner_id")? != provider
            || quote.try_get::<i64, _>("max_amount")? != held
            || fact.try_get::<String, _>("response_digest")?
                != fact.try_get::<String, _>("committed_digest")?
            || fact.try_get::<String, _>("request_projection_digest")?
                != quote.try_get::<String, _>("projection_digest")?
            || fact.try_get::<String, _>("meter_profile_digest")?
                != quote.try_get::<String, _>("meter_profile_digest")?
            || fact.try_get::<i64, _>("input_units")? != quote.try_get::<i64, _>("input_units")?
        {
            return Err(ArtifactError::Integrity);
        }
        let credit = |n| Credits::new(n).map_err(|_| ArtifactError::Integrity);
        let pricing = TextQuote {
            input_units: quote.try_get::<i64, _>("input_units")? as u64,
            output_limit: quote.try_get::<i64, _>("output_limit")? as u64,
            input_price: credit(quote.try_get("input_price")?)?,
            output_price: credit(quote.try_get("output_price")?)?,
        };
        if pricing
            .maximum()
            .map_err(|_| ArtifactError::Integrity)?
            .units()
            != held
        {
            return Err(ArtifactError::Integrity);
        }
        let charge = pricing
            .charge(
                TextDelivery::CommittedSuccess,
                fact.try_get::<i64, _>("output_units")? as u64,
                fact.try_get("has_tools")?,
            )
            .map_err(|_| ArtifactError::Integrity)?;
        let (fee, earned) =
            orange_domain::split_fee(charge, quote.try_get::<i32, _>("fee_bps")? as u16)
                .map_err(|_| ArtifactError::Integrity)?;
        let next = crate::store::budget_model(&root)
            .map_err(|_| ArtifactError::Integrity)?
            .settle(credit(held)?, charge)
            .map_err(|_| ArtifactError::Integrity)?;
        let held_buckets = [
            row.try_get::<i64, _>("held_gift")?,
            row.try_get::<i64, _>("held_earned")?,
            row.try_get::<i64, _>("held_purchased")?,
        ];
        let used = crate::store::allocate(charge.units(), held_buckets)
            .map_err(|_| ArtifactError::Integrity)?;
        sqlx::query("UPDATE wallets SET gift=gift+$2,earned=earned+$3,purchased=purchased+$4,reserved_gift=reserved_gift-$5,reserved_earned=reserved_earned-$6,reserved_purchased=reserved_purchased-$7 WHERE owner_id=$1").bind(&owner).bind(held_buckets[0]-used[0]).bind(held_buckets[1]-used[1]).bind(held_buckets[2]-used[2]).bind(held_buckets[0]).bind(held_buckets[1]).bind(held_buckets[2]).execute(&mut *tx).await?;
        sqlx::query("UPDATE wallets SET pending_earned=pending_earned+$2 WHERE owner_id=$1")
            .bind(&provider)
            .bind(earned.units())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE platform_credit_balances SET balance=balance+$1 WHERE account='fees'")
            .bind(fee.units())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE budget_roots SET spent=$2,reserved=$3 WHERE root_budget_id=$1")
            .bind(&root_id)
            .bind(next.spent.units())
            .bind(next.reserved.units())
            .execute(&mut *tx)
            .await?;
        let mut entries = vec![];
        for (i, bucket) in ["gift", "earned", "purchased"].iter().enumerate() {
            entries.push((format!("{owner}:{bucket}:reserved"), -held_buckets[i]));
            entries.push((
                format!("{owner}:{bucket}:available"),
                held_buckets[i] - used[i],
            ));
        }
        entries.push((format!("{provider}:earned:pending"), earned.units()));
        entries.push(("platform:fees".into(), fee.units()));
        let key = format!("settle:{job}");
        crate::store::ledger(&mut tx, &owner, &key, entries)
            .await
            .map_err(|e| match e {
                crate::store::StoreError::Database(e) => ArtifactError::Database(e),
                _ => ArtifactError::Integrity,
            })?;
        let transaction: String = sqlx::query_scalar(
            "SELECT transaction_id FROM ledger_transactions WHERE business_key=$1",
        )
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO job_settlements(job_id,transaction_id,charge,platform_fee,provider_pending,provider_owner_id) VALUES($1,$2,$3,$4,$5,$6)").bind(job).bind(&transaction).bind(charge.units()).bind(fee.units()).bind(earned.units()).bind(&provider).execute(&mut *tx).await?;
        sqlx::query("UPDATE jobs SET settlement_state='settled',charge=$2 WHERE job_id=$1")
            .bind(job)
            .bind(charge.units())
            .execute(&mut *tx)
            .await?;
        crate::events::append(&mut tx, &owner, job, "job.settled", &transaction).await?;
        tx.commit().await?;
        Ok(true)
    }
    /// Server-owned retry of already committed facts, not a public payment API.
    pub async fn settle_ready(&self, limit: u16) -> Result<(u32, u32), ArtifactError> {
        if !(1..=100).contains(&limit) {
            return Err(ArtifactError::Invalid);
        }
        let jobs:Vec<String>=sqlx::query_scalar("SELECT j.job_id FROM jobs j JOIN job_usage_facts u USING(job_id) JOIN worker_output_uploads w USING(job_id) JOIN artifacts a ON a.artifact_id=w.artifact_id WHERE j.execution_state='succeeded' AND j.settlement_state='pending' AND a.root_id=$1 ORDER BY u.measured_at LIMIT $2").bind(&self.inner.objects.root_id).bind(i64::from(limit)).fetch_all(&self.inner.pool).await?;
        let mut settled = 0;
        let mut failed = 0;
        for job in jobs {
            match self.settle_committed(&job).await {
                Ok(true) => settled += 1,
                Ok(false) => {}
                Err(_) => failed += 1,
            }
        }
        Ok((settled, failed))
    }
}
