//! Development audio rates reviewed against an independently observed CPU profile.
use super::*;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTerms {
    scope: Scope,
    observation_id: String,
    price_per_minute: Money,
    fee_bps: u16,
}
impl AudioTerms {
    fn validate(&self) -> Result<(), AdminError> {
        valid_scope(&self.scope)?;
        if !id(&self.observation_id) || self.fee_bps > 10000 || self.price_per_minute.0.units() == 0
        {
            return Err(AdminError::Invalid);
        }
        orange_domain::price_units(600000, self.price_per_minute.0, 60000)
            .map_err(|_| AdminError::Invalid)?;
        Ok(())
    }
}
pub(super) fn terms(row: &sqlx::postgres::PgRow) -> Result<AudioTerms, AdminError> {
    Ok(AudioTerms {
        scope: scope_of(row)?,
        observation_id: row.try_get("observation_id")?,
        price_per_minute: Money(
            orange_domain::Credits::new(row.try_get("price_per_minute")?)
                .map_err(|_| AdminError::Unavailable)?,
        ),
        fee_bps: u16::try_from(row.try_get::<i32, _>("fee_bps")?)
            .map_err(|_| AdminError::Unavailable)?,
    })
}
async fn binding(tx: &mut Tx<'_>, t: &AudioTerms) -> Result<sqlx::postgres::PgRow, AdminError> {
    t.validate()?;
    sqlx::query("SELECT n.owner_id,o.node_id,n.region_id,o.execution_profile_id,o.execution_profile_digest,o.observation_digest,i.python_sha256
      FROM worker_transcription_profile_observations o JOIN transcription_process_identities i USING(observation_id)
      JOIN nodes n USING(node_id) JOIN owners b ON b.owner_id=n.owner_id JOIN wallets w ON w.owner_id=n.owner_id
      JOIN reviewed_execution_profiles p USING(execution_profile_id)
      WHERE o.observation_id=$1 AND n.environment='dev' AND n.state<>'revoked' AND b.state='active' AND ($2::text IS NULL OR n.owner_id=$2)
      AND p.enabled AND p.model_version_id=o.model_version AND p.model_sha256=o.model_sha256 AND p.execution_profile_digest=o.execution_profile_digest FOR SHARE OF n,b,p")
      .bind(&t.observation_id).bind((t.scope.kind=="tenant").then_some(&t.scope.scope_id)).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)
}
fn hashes(
    price: &str,
    t: &AudioTerms,
    row: &sqlx::postgres::PgRow,
) -> Result<(String, String), AdminError> {
    let target = digest(
        "admin.audio-price.terms.v1",
        &(
            price,
            t,
            row.try_get::<&str, _>("owner_id")?,
            row.try_get::<&str, _>("node_id")?,
            row.try_get::<&str, _>("execution_profile_id")?,
        ),
    )?;
    let review = digest(
        "admin.audio-price.review.v1",
        &(
            &target,
            row.try_get::<&str, _>("region_id")?,
            row.try_get::<&str, _>("observation_digest")?,
            row.try_get::<&str, _>("execution_profile_digest")?,
            row.try_get::<&str, _>("python_sha256")?,
            "decoded_milliseconds_ceil_per_minute.v1",
        ),
    )?;
    Ok((target, review))
}
pub async fn create(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    body: Result<Json<AudioTerms>, JsonRejection>,
) -> Result<Response, AdminError> {
    let t = body.map_err(|_| AdminError::Invalid)?.0;
    t.validate()?;
    let key = key(&headers)?;
    let mut tx = begin(&identity).await?;
    let session = authenticate(&identity, &headers, &mut tx).await?;
    let who = &session.view.principal_id;
    let deadline = actor(&mut tx, who, &t.scope, &["pricing.draft"]).await?;
    let hash = digest("admin.audio-price.request.v1", &t)?;
    let lock = digest("admin.audio-price.key.v1", &(who, key))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(lock)
        .execute(&mut *tx)
        .await?;
    let old:Option<(String,String)>=sqlx::query_as("SELECT price_version,request_digest FROM admin_transcription_price_drafts WHERE created_by=$1 AND request_key=$2").bind(who).bind(key).fetch_optional(&mut *tx).await?;
    let price = if let Some((price, prior)) = old {
        if prior != hash {
            return Err(AdminError::Idempotency);
        }
        price
    } else {
        let row = binding(&mut tx, &t).await?;
        let price = format!("asr-price-{}", uuid::Uuid::new_v4());
        let (target, review) = hashes(&price, &t, &row)?;
        sqlx::query("INSERT INTO admin_transcription_price_drafts(price_version,created_by,request_key,request_digest,scope_kind,scope_id,provider_owner_id,node_id,execution_profile_id,observation_id,price_per_minute,fee_bps,terms_digest,review_report_id,created_at)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,floor(extract(epoch FROM clock_timestamp()))::bigint)")
            .bind(&price).bind(who).bind(key).bind(hash).bind(&t.scope.kind).bind(&t.scope.scope_id).bind(row.try_get::<&str,_>("owner_id")?).bind(row.try_get::<&str,_>("node_id")?)
            .bind(row.try_get::<&str,_>("execution_profile_id")?).bind(&t.observation_id).bind(t.price_per_minute.0.units()).bind(i32::from(t.fee_bps)).bind(target).bind(review).execute(&mut *tx).await?;
        audit(
            &mut tx,
            who,
            &t.scope,
            "pricing.transcription_draft_created",
            Some(&price),
        )
        .await?;
        price
    };
    let value = project(&one(&mut tx, &t.scope, &price).await?)?;
    finish(&identity, &headers, &mut tx, deadline).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

pub(super) async fn verify(
    tx: &mut Tx<'_>,
    price: &str,
    row: &sqlx::postgres::PgRow,
) -> Result<(), AdminError> {
    let t = terms(row)?;
    let bound = binding(tx, &t).await?;
    let (target, review) = hashes(price, &t, &bound)?;
    if target != row.try_get::<&str, _>("terms_digest")?
        || review != row.try_get::<&str, _>("review_report_id")?
    {
        return Err(AdminError::Conflict);
    }
    Ok(())
}
pub(super) async fn activate(
    tx: &mut Tx<'_>,
    row: &sqlx::postgres::PgRow,
    price: &str,
    scope: &Scope,
) -> Result<bool, AdminError> {
    let deadline = activation_deadline(tx, row, scope).await?;
    verify(tx, price, row).await?;
    // Protect any already active schedule. A draft cannot implicitly retire another version.
    let changed=sqlx::query("INSERT INTO transcription_prices(price_version,node_id,execution_profile_id,price_per_minute,fee_bps,review_digest,admission,enabled)
        SELECT $1,$2,$3,$4,$5,$6,'development_only',true WHERE $7>floor(extract(epoch FROM clock_timestamp()))::bigint ON CONFLICT DO NOTHING")
        .bind(price).bind(row.try_get::<&str,_>("node_id")?).bind(row.try_get::<&str,_>("execution_profile_id")?).bind(row.try_get::<i64,_>("price_per_minute")?)
        .bind(row.try_get::<i32,_>("fee_bps")?).bind(row.try_get::<&str,_>("review_report_id")?).bind(deadline).execute(&mut **tx).await?;
    Ok(changed.rows_affected() == 1)
}
