//! Trusted development-only terms; duration comes from decoded immutable grant evidence.
use super::*;
use orange_domain::{Credits, price_units};
use orange_protocol::{
    Money,
    pricing::{CreateTranscriptionQuote, TranscriptionPrice, TranscriptionQuoteView},
};

fn money(n: i64) -> Result<Money, ArtifactError> {
    Ok(Money(
        Credits::new(n).map_err(|_| ArtifactError::Integrity)?,
    ))
}
fn maximum(ms: i64, rate: i64) -> Result<Credits, ArtifactError> {
    if !(1..=600000).contains(&ms) || rate <= 0 {
        return Err(ArtifactError::Invalid);
    }
    price_units(ms as u64, money(rate)?.0, 60000).map_err(|_| ArtifactError::Budget)
}
async fn active_data(
    tx: &mut Tx<'_>,
    owner: &str,
    user: &str,
    id: &str,
) -> Result<(sqlx::postgres::PgRow, sqlx::postgres::PgRow), ArtifactError> {
    let observed:String=sqlx::query_scalar("SELECT observation_id FROM transcription_input_grants WHERE owner_id=$1 AND grant_id=$2 AND author_id=$3")
        .bind(owner).bind(id).bind(user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
    let target = audio_grants::observed_target(tx, &observed).await?;
    if target.try_get::<i32, _>("transcription_output_version")? != 1
        || target.try_get::<i32, _>("cpu_cleanup_version")? != 1
    {
        return Err(ArtifactError::PricingUnavailable);
    }
    sqlx::query("SELECT grant_id FROM transcription_input_grants WHERE owner_id=$1 AND grant_id=$2 FOR SHARE").bind(owner).bind(id).fetch_one(&mut **tx).await?;
    let grant = audio_grants::lookup(tx, owner, id, None)
        .await?
        .ok_or(ArtifactError::Forbidden)?;
    if grant.try_get::<String, _>("grant_state")? != "active"
        || grant.try_get::<String, _>("region_id")? != target.try_get::<String, _>("region_id")?
    {
        return Err(ArtifactError::Forbidden);
    }
    let budget: &str = grant.try_get("root_budget_id")?;
    crate::store::check_budget_actor(tx, user, owner, budget)
        .await
        .map_err(|_| ArtifactError::Forbidden)?;
    let source=sqlx::query("SELECT a.*,u.upload_id FROM artifact_uploads u JOIN artifacts a USING(artifact_id) WHERE u.owner_id=$1 AND u.upload_id=$2 FOR SHARE OF a")
        .bind(owner).bind(grant.try_get::<&str,_>("upload_id")?).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
    audio_grants::checked_source_row(tx, user, owner, &grant, &source).await?;
    Ok((grant, target))
}
async fn price(
    tx: &mut Tx<'_>,
    target: &sqlx::postgres::PgRow,
    version: Option<&str>,
) -> Result<Option<sqlx::postgres::PgRow>, ArtifactError> {
    Ok(sqlx::query("SELECT p.* FROM transcription_prices p JOIN wallets w ON w.owner_id=$3
        WHERE p.node_id=$1 AND p.execution_profile_id=$2 AND p.enabled AND p.admission='development_only' AND ($4::text IS NULL OR p.price_version=$4)
        FOR SHARE OF p").bind(target.try_get::<&str,_>("node_id")?).bind(target.try_get::<&str,_>("execution_profile_id")?)
        .bind(target.try_get::<&str,_>("provider_owner_id")?).bind(version).fetch_optional(&mut **tx).await?)
}
async fn existing(
    tx: &mut Tx<'_>,
    owner: &str,
    request: &str,
) -> Result<Option<sqlx::postgres::PgRow>, ArtifactError> {
    Ok(sqlx::query("SELECT t.*,q.root_budget_id,q.invocation_id,q.projection_digest,q.max_amount,floor(extract(epoch FROM q.expires_at))::bigint AS expires
         FROM transcription_quote_terms t JOIN quotes q USING(quote_id) WHERE t.owner_id=$1 AND t.request_id=$2")
        .bind(owner).bind(request).fetch_optional(&mut **tx).await?)
}
fn view_quote(row: &sqlx::postgres::PgRow) -> Result<TranscriptionQuoteView, ArtifactError> {
    Ok(TranscriptionQuoteView {
        quote_id: row.try_get("quote_id")?,
        grant_id: row.try_get("grant_id")?,
        root_budget_id: row.try_get("root_budget_id")?,
        invocation_id: row.try_get("invocation_id")?,
        request_projection_digest: row.try_get("projection_digest")?,
        price_version: row.try_get("price_version")?,
        observation_id: row.try_get("observation_id")?,
        duration_ms: row.try_get::<i64, _>("duration_ms")?.to_string(),
        price_per_minute: money(row.try_get("price_per_minute")?)?,
        max_amount: money(row.try_get("max_amount")?)?,
        fee_bps: row.try_get::<i32, _>("fee_bps")? as u16,
        expires_at_unix: row.try_get::<i64, _>("expires")?.to_string(),
    })
}
impl ArtifactStore {
    pub(super) async fn audio_prices(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        grant_id: &str,
    ) -> Result<Vec<TranscriptionPrice>, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let (grant, target) = active_data(&mut tx, owner, &actor.user, grant_id).await?;
        sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE")
            .bind(grant.try_get::<&str,_>("root_budget_id")?).bind(owner).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        let rows = match price(&mut tx, &target, None).await? {
            Some(r) => vec![TranscriptionPrice {
                price_version: r.try_get("price_version")?,
                price_per_minute: money(r.try_get("price_per_minute")?)?,
                fee_bps: r.try_get::<i32, _>("fee_bps")? as u16,
            }],
            None => vec![],
        };
        authority::finish(tx, actor).await?;
        Ok(rows)
    }
    pub(super) async fn audio_quote(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        req: &CreateTranscriptionQuote,
    ) -> Result<TranscriptionQuoteView, ArtifactError> {
        if ![&req.request_id, &req.grant_id, &req.price_version]
            .iter()
            .all(|s| valid_id(s))
        {
            return Err(ArtifactError::Invalid);
        }
        let request =
            request_digest("transcription.quote.v1", req).map_err(|_| ArtifactError::Invalid)?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        let key = request_digest(
            "transcription.quote.request-key.v1",
            &(owner, &req.request_id),
        )
        .map_err(|_| ArtifactError::Invalid)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        let (grant, target) = active_data(&mut tx, owner, &actor.user, &req.grant_id).await?;
        let terms = price(&mut tx, &target, Some(&req.price_version))
            .await?
            .ok_or(ArtifactError::PricingUnavailable)?;
        if let Some(row) = existing(&mut tx, owner, &req.request_id).await? {
            if row.try_get::<String, _>("request_digest")? != request
                || row.try_get::<String, _>("requester_id")? != actor.user
            {
                return Err(ArtifactError::Conflict);
            }
            let result = view_quote(&row)?;
            authority::finish(tx, actor).await?;
            return Ok(result);
        }
        let duration = grant.try_get::<serde_json::Value, _>("measurement")?["duration_ms"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or(ArtifactError::Integrity)?;
        let rate: i64 = terms.try_get("price_per_minute")?;
        let amount = maximum(duration, rate)?;
        let budget: &str = grant.try_get("root_budget_id")?;
        let invoke: &str = grant.try_get("invocation_id")?;
        let projection: &str = grant.try_get("projection_digest")?;
        let root=sqlx::query("SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE")
            .bind(budget).bind(owner).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        crate::store::budget_model(&root)
            .map_err(|_| ArtifactError::Integrity)?
            .reserve(amount)
            .map_err(|_| ArtifactError::Budget)?;
        let quote = id();
        let inserted=sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
            SELECT $1,$2,$3,$4,$5,$6,$7,date_trunc('second',least(clock_timestamp()+interval '60 seconds',expires_at))
            FROM transcription_input_grants WHERE owner_id=$2 AND grant_id=$8 AND expires_at>clock_timestamp()+interval '1 second'")
            .bind(&quote).bind(owner).bind(budget).bind(invoke).bind(projection).bind(&req.price_version).bind(amount.units()).bind(&req.grant_id)
            .execute(&mut *tx).await?.rows_affected();
        if inserted != 1 {
            return Err(ArtifactError::Expired);
        }
        sqlx::query("INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline)
            SELECT $1,$2,$3,$4,date_trunc('second',least(clock_timestamp()+interval '10 minutes',expires_at))
            FROM transcription_input_grants WHERE owner_id=$5 AND grant_id=$6")
            .bind(&quote).bind(target.try_get::<&str,_>("node_id")?).bind(target.try_get::<&str,_>("execution_profile_id")?)
            .bind(projection).bind(owner).bind(&req.grant_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO transcription_quote_terms(quote_id,owner_id,grant_id,requester_id,request_id,request_digest,price_version,provider_owner_id,
            duration_ms,price_per_minute,fee_bps,observation_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
            .bind(&quote).bind(owner).bind(&req.grant_id).bind(&actor.user).bind(&req.request_id).bind(request).bind(&req.price_version)
            .bind(target.try_get::<&str,_>("provider_owner_id")?).bind(duration).bind(rate).bind(terms.try_get::<i32,_>("fee_bps")?).bind(grant.try_get::<&str,_>("observation_id")?)
            .execute(&mut *tx).await?;
        let result = view_quote(
            &existing(&mut tx, owner, &req.request_id)
                .await?
                .ok_or(ArtifactError::Integrity)?,
        )?;
        crate::events::append(&mut tx, owner, &quote, "quote.created", projection).await?;
        authority::finish(tx, actor).await?;
        Ok(result)
    }
}

/// Validate all mutable admissions for a new effect. Previously committed accounting uses only frozen facts.
pub(crate) async fn admitted(
    tx: &mut Tx<'_>,
    quote: &str,
    owner: &str,
    user: &str,
) -> Result<(), ArtifactError> {
    let terms=sqlx::query("SELECT * FROM transcription_quote_terms WHERE quote_id=$1 AND owner_id=$2 AND requester_id=$3")
        .bind(quote).bind(owner).bind(user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
    let (grant, target) = active_data(tx, owner, user, terms.try_get("grant_id")?).await?;
    price(tx, &target, Some(terms.try_get("price_version")?))
        .await?
        .ok_or(ArtifactError::PricingUnavailable)?;
    if target.try_get::<String, _>("provider_owner_id")?
        != terms.try_get::<String, _>("provider_owner_id")?
        || grant.try_get::<String, _>("observation_id")?
            != terms.try_get::<String, _>("observation_id")?
    {
        return Err(ArtifactError::Forbidden);
    }
    Ok(())
}

pub(super) async fn committed_charge(
    tx: &mut Tx<'_>,
    job: &str,
    held: i64,
) -> Result<(Credits, u16), ArtifactError> {
    let row=sqlx::query("SELECT t.duration_ms,t.price_per_minute,t.fee_bps FROM jobs j JOIN quotes q USING(quote_id)
        JOIN transcription_quote_terms t USING(quote_id) JOIN transcription_response_facts f USING(job_id)
        JOIN job_response_commits c USING(job_id)
        WHERE j.job_id=$1 AND j.purpose='transcription' AND j.execution_state='succeeded'
        AND j.owner_id=q.owner_id AND t.owner_id=j.owner_id AND f.owner_id=j.owner_id AND j.submitting_user_id=t.requester_id
        AND q.root_budget_id=j.root_budget_id AND q.invocation_id=j.invocation_id AND q.price_version=t.price_version
        AND q.max_amount=j.reserved AND f.grant_id=t.grant_id AND f.observation_id=t.observation_id
        AND f.projection_digest=q.projection_digest AND f.duration_ms=t.duration_ms AND f.response_digest=c.response_digest")
        .bind(job).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Integrity)?;
    let charge = maximum(
        row.try_get("duration_ms")?,
        row.try_get("price_per_minute")?,
    )?;
    if charge.units() != held {
        return Err(ArtifactError::Integrity);
    }
    Ok((charge, row.try_get::<i32, _>("fee_bps")? as u16))
}
