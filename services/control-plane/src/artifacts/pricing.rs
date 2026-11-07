use super::*;
use orange_domain::{Budget, Credits, TextQuote};
use orange_model_runtime::meter_service::{MeasuredOutput, MeterError, TrustedMeter};
fn meter_error(error: MeterError) -> ArtifactError {
    match error {
        MeterError::Invalid | MeterError::Limit => ArtifactError::Invalid,
        MeterError::Busy => ArtifactError::Busy,
        MeterError::Unavailable => ArtifactError::PricingUnavailable,
    }
}
use orange_protocol::{
    Money,
    pricing::{CreateTextQuote, TextPriceView, TextQuoteView},
};
fn money(n: i64) -> Result<Money, ArtifactError> {
    Ok(Money(
        Credits::new(n).map_err(|_| ArtifactError::Integrity)?,
    ))
}
fn quote_view(row: &sqlx::postgres::PgRow) -> Result<TextQuoteView, ArtifactError> {
    Ok(TextQuoteView {
        quote_id: row.try_get("quote_id")?,
        root_budget_id: row.try_get("root_budget_id")?,
        invocation_id: row.try_get("invocation_id")?,
        request_projection_digest: row.try_get("projection_digest")?,
        price_version: row.try_get("price_version")?,
        meter_profile_digest: row.try_get("meter_profile_digest")?,
        input_units: row.try_get::<i64, _>("input_units")?.to_string(),
        output_limit: row.try_get::<i64, _>("output_limit")?.to_string(),
        max_amount: money(row.try_get("max_amount")?)?,
        expires_at_unix: row.try_get::<i64, _>("expires")?.to_string(),
    })
}
async fn price_in(
    tx: &mut Tx<'_>,
    version: &str,
    meter: &TrustedMeter,
) -> Result<sqlx::postgres::PgRow, ArtifactError> {
    if !meter.available() {
        return Err(ArtifactError::PricingUnavailable);
    }
    let d = meter.descriptor();
    let node: String = sqlx::query_scalar("SELECT node_id FROM text_prices WHERE price_version=$1")
        .bind(version)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ArtifactError::PricingUnavailable)?;
    sqlx::query("SELECT node_id FROM nodes WHERE node_id=$1 FOR SHARE")
        .bind(&node)
        .fetch_one(&mut **tx)
        .await?;

    let row=sqlx::query("SELECT p.*,r.model_version_id,r.model_sha256,r.execution_profile_digest,n.owner_id AS provider_owner_id FROM text_prices p JOIN trusted_meter_profiles m ON m.profile_digest=p.meter_profile_digest JOIN reviewed_execution_profiles r USING(execution_profile_id) JOIN nodes n USING(node_id) JOIN owners o ON o.owner_id=n.owner_id JOIN wallets w ON w.owner_id=n.owner_id JOIN worker_sessions s USING(node_id) JOIN worker_credentials c ON c.certificate_sha256=s.certificate_sha256 JOIN users u ON u.user_id=c.enrollment_user_id WHERE p.price_version=$1 AND p.enabled AND p.admission='development_only' AND m.enabled AND m.profile_digest=$2 AND m.descriptor=$3 AND r.enabled AND r.execution_profile_digest=$4 AND r.model_sha256=$5 AND r.model_version_id=$6 AND n.environment='dev' AND n.state='online' AND o.state='active' AND s.expires_at>clock_timestamp() AND s.output_version=1 AND c.active AND c.not_before<=clock_timestamp() AND c.expires_at>clock_timestamp() AND u.active AND (u.personal_owner_id=n.owner_id OR EXISTS(SELECT 1 FROM memberships ms WHERE ms.organization_id=n.owner_id AND ms.user_id=u.user_id AND ms.active AND ms.role IN('owner','admin'))) FOR SHARE OF p,m,r,n,o,s,c,u")
        .bind(version).bind(&d.profile_digest).bind(serde_json::to_value(d).map_err(|_|ArtifactError::Invalid)?).bind(&d.execution_digest).bind(&d.model_sha256).bind(&d.model_version).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::PricingUnavailable)?;
    Ok(row)
}
impl ArtifactStore {
    pub async fn prices(&self, actor: &ArtifactActor) -> Result<Vec<TextPriceView>, ArtifactError> {
        let meter = self
            .inner
            .meter
            .as_ref()
            .ok_or(ArtifactError::PricingUnavailable)?;
        let mut tx = self.inner.pool.begin().await?;
        let personal: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1")
                .bind(&actor.user)
                .fetch_one(&mut *tx)
                .await?;
        authority::scope(&mut tx, actor, &personal, false).await?;
        let versions:Vec<String>=sqlx::query_scalar("SELECT price_version FROM text_prices WHERE enabled AND meter_profile_digest=$1 ORDER BY price_version LIMIT 100").bind(&meter.descriptor().profile_digest).fetch_all(&mut *tx).await?;
        let mut result = vec![];
        for version in versions {
            match price_in(&mut tx, &version, meter).await {
                Ok(row) => result.push(TextPriceView {
                    price_version: version,
                    model_version: row.try_get("model_version_id")?,
                    node_id: row.try_get("node_id")?,
                    meter_profile_digest: row.try_get("meter_profile_digest")?,
                    input_price: money(row.try_get("input_price")?)?,
                    output_price: money(row.try_get("output_price")?)?,
                    output_limit: row.try_get::<i64, _>("output_limit")?.to_string(),
                    fee_bps: row.try_get::<i32, _>("fee_bps")? as u16,
                }),
                Err(ArtifactError::NotReady | ArtifactError::PricingUnavailable) => {}
                Err(e) => return Err(e),
            }
        }
        authority::finish(tx, actor).await?;
        Ok(result)
    }
    pub async fn create_text_quote(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        r: &CreateTextQuote,
    ) -> Result<TextQuoteView, ArtifactError> {
        if !r.valid() {
            return Err(ArtifactError::Invalid);
        }
        let meter = self
            .inner
            .meter
            .as_ref()
            .ok_or(ArtifactError::PricingUnavailable)?;
        let digest = request_digest("text.quote.v1", r).map_err(|_| ArtifactError::Invalid)?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        crate::store::check_budget_actor(&mut tx, &actor.user, owner, &r.root_budget_id)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let prior=sqlx::query("SELECT q.*,m.*,floor(extract(epoch FROM q.expires_at))::bigint AS expires FROM quotes q JOIN quote_metering m USING(quote_id) WHERE m.owner_id=$1 AND m.request_id=$2").bind(owner).bind(&r.request_id).fetch_optional(&mut *tx).await?;
        if let Some(prior) = prior {
            if prior.try_get::<String, _>("requester_id")? != actor.user
                || prior.try_get::<String, _>("request_digest")? != digest
            {
                return Err(ArtifactError::Conflict);
            }
            let view = quote_view(&prior)?;
            authority::finish(tx, actor).await?;
            return Ok(view);
        }
        price_in(&mut tx, &r.price_version, meter).await?;
        let root:Option<String>=sqlx::query_scalar("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE").bind(&r.root_budget_id).bind(owner).fetch_optional(&mut *tx).await?;
        if root.is_none() {
            return Err(ArtifactError::Forbidden);
        }
        authority::finish(tx, actor).await?;
        let (input, bytes, stamp) = self
            .read(actor, owner, &r.input_upload_id, &r.input_version_id)
            .await?;
        if input.media_type != "application/json" || bytes.len() > 256 * 1024 {
            return Err(ArtifactError::Invalid);
        }
        let value = orange_protocol::canonical::parse_tool_arguments(
            std::str::from_utf8(&bytes.to_vec()).map_err(|_| ArtifactError::Invalid)?,
        )
        .map_err(|_| ArtifactError::Invalid)?;
        let request: orange_protocol::model::GenerateRequest =
            serde_json::from_value(value).map_err(|_| ArtifactError::Invalid)?;
        request.validate().map_err(|_| ArtifactError::Invalid)?;
        let measured = meter.input(request.clone()).await.map_err(meter_error)?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        crate::store::check_budget_actor(&mut tx, &actor.user, owner, &r.root_budget_id)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let price = price_in(&mut tx, &r.price_version, meter).await?;
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(&r.root_budget_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ArtifactError::Forbidden)?;
        // Root lock serializes same-owner quote request races without holding
        // any DB transaction while counting with the owned backend.
        let prior=sqlx::query("SELECT q.*,m.*,floor(extract(epoch FROM q.expires_at))::bigint AS expires FROM quotes q JOIN quote_metering m USING(quote_id) WHERE m.owner_id=$1 AND m.request_id=$2").bind(owner).bind(&r.request_id).fetch_optional(&mut *tx).await?;
        if let Some(prior) = prior {
            if prior.try_get::<String, _>("request_digest")? != digest
                || prior.try_get::<String, _>("requester_id")? != actor.user
            {
                return Err(ArtifactError::Conflict);
            }
            let v = quote_view(&prior)?;
            authority::finish(tx, actor).await?;
            return Ok(v);
        }
        if i64::from(r.output_limit) > price.try_get::<i64, _>("output_limit")?
            || measured.profile_digest() != meter.descriptor().profile_digest
        {
            return Err(ArtifactError::Invalid);
        }
        let current = self.row_in(&mut tx, owner, &r.input_upload_id).await?;
        authority::resource(&mut tx, actor, owner, &current, false, false).await?;
        if view(&current)? != input || authority::stamp(&mut tx, &current).await? != stamp {
            return Err(ArtifactError::Forbidden);
        }
        let pricing = TextQuote {
            input_units: measured.units(),
            output_limit: u64::from(r.output_limit),
            input_price: money(price.try_get("input_price")?)?.0,
            output_price: money(price.try_get("output_price")?)?.0,
        };
        let maximum = pricing.maximum().map_err(|_| ArtifactError::Budget)?;
        Budget {
            limit: money(root.try_get("limit_amount")?)?.0,
            spent: money(root.try_get("spent")?)?.0,
            reserved: money(root.try_get("reserved")?)?.0,
            closing: root.try_get::<String, _>("state")? != "active",
        }
        .reserve(maximum)
        .map_err(|_| ArtifactError::Budget)?;
        let quote = id();
        let expires:i64=sqlx::query_scalar("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+interval '60 seconds') RETURNING floor(extract(epoch FROM expires_at))::bigint").bind(&quote).bind(owner).bind(&r.root_budget_id).bind(&request.invocation_id).bind(measured.request_digest()).bind(&r.price_version).bind(maximum.units()).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline) VALUES($1,$2,$3,$4,date_trunc('second',clock_timestamp()+interval '5 minutes'))").bind(&quote).bind(price.try_get::<String,_>("node_id")?).bind(price.try_get::<String,_>("execution_profile_id")?).bind(measured.request_digest()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO quote_metering(quote_id,request_id,owner_id,requester_id,request_digest,input_artifact_id,input_version_id,input_units,output_limit,price_version,meter_profile_digest,provider_owner_id,input_price,output_price,fee_bps) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)").bind(&quote).bind(&r.request_id).bind(owner).bind(&actor.user).bind(&digest).bind(&input.artifact_id).bind(&input.version_id).bind(measured.units() as i64).bind(i64::from(r.output_limit)).bind(&r.price_version).bind(measured.profile_digest()).bind(price.try_get::<String,_>("provider_owner_id")?).bind(pricing.input_price.units()).bind(pricing.output_price.units()).bind(price.try_get::<i32,_>("fee_bps")?).execute(&mut *tx).await?;
        crate::events::append(&mut tx, owner, &quote, "quote.created", &digest).await?;
        authority::finish(tx, actor).await?;
        Ok(TextQuoteView {
            quote_id: quote,
            root_budget_id: r.root_budget_id.clone(),
            invocation_id: request.invocation_id,
            request_projection_digest: measured.request_digest().into(),
            price_version: r.price_version.clone(),
            meter_profile_digest: measured.profile_digest().into(),
            input_units: measured.units().to_string(),
            output_limit: r.output_limit.to_string(),
            max_amount: Money(maximum),
            expires_at_unix: expires.to_string(),
        })
    }
    pub(super) async fn measure_job_output(
        &self,
        job: &str,
        request: orange_protocol::model::GenerateRequest,
        candidate: orange_protocol::model::ResponseCandidate,
    ) -> Result<Option<MeasuredOutput>, ArtifactError> {
        let row = sqlx::query(
            "SELECT m.* FROM jobs j JOIN quote_metering m USING(quote_id) WHERE j.job_id=$1",
        )
        .bind(job)
        .fetch_optional(&self.inner.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let meter = self
            .inner
            .meter
            .as_ref()
            .ok_or(ArtifactError::PricingUnavailable)?;
        if row.try_get::<String, _>("meter_profile_digest")? != meter.descriptor().profile_digest {
            return Err(ArtifactError::Forbidden);
        }
        let fact = meter
            .output(
                request,
                candidate,
                row.try_get::<i64, _>("output_limit")? as u64,
            )
            .await
            .map_err(meter_error)?;
        if fact.input().units() != row.try_get::<i64, _>("input_units")? as u64 {
            return Err(ArtifactError::Integrity);
        }
        Ok(Some(fact))
    }
    pub(super) async fn record_usage(
        &self,
        tx: &mut Tx<'_>,
        job: &str,
        response: &str,
        fact: Option<&MeasuredOutput>,
    ) -> Result<(), ArtifactError> {
        let snapshot=sqlx::query("SELECT m.*,q.projection_digest,q.max_amount FROM jobs j JOIN quotes q USING(quote_id) JOIN quote_metering m USING(quote_id) WHERE j.job_id=$1").bind(job).fetch_optional(&mut **tx).await?;
        let Some(snapshot) = snapshot else {
            return if fact.is_none() {
                Ok(())
            } else {
                Err(ArtifactError::Integrity)
            };
        };
        let fact = fact.ok_or(ArtifactError::PricingUnavailable)?;
        let meter = self
            .inner
            .meter
            .as_ref()
            .ok_or(ArtifactError::PricingUnavailable)?;
        price_in(tx, &snapshot.try_get::<String, _>("price_version")?, meter).await?;
        if fact.input().profile_digest() != snapshot.try_get::<String, _>("meter_profile_digest")?
            || fact.input().request_digest()
                != snapshot.try_get::<String, _>("projection_digest")?
            || fact.response_digest() != response
            || fact.input().units() != snapshot.try_get::<i64, _>("input_units")? as u64
            || fact.units() > snapshot.try_get::<i64, _>("output_limit")? as u64
        {
            return Err(ArtifactError::Integrity);
        }
        sqlx::query("INSERT INTO job_usage_facts(job_id,meter_profile_digest,request_projection_digest,response_digest,output_projection_digest,input_units,output_units,has_tools) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(job).bind(fact.input().profile_digest()).bind(fact.input().request_digest()).bind(response).bind(fact.projection_digest()).bind(fact.input().units() as i64).bind(fact.units() as i64).bind(fact.has_tools()).execute(&mut **tx).await?;
        Ok(())
    }
}
