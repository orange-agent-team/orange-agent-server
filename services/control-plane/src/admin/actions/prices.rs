//! Immutable development price terms. Submission, independent approval and
//! the time at which a version becomes quotable have distinct receipts.
pub mod audio;
use super::*;
use orange_domain::TextQuote;
use orange_protocol::Money;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

// These two compiled SQL literals share the statement shape. Caller input is always bound.
macro_rules! catalog_part {
    (audio,drafts) => {
        "admin_transcription_price_drafts"
    };
    (text,drafts) => {
        "admin_text_price_drafts"
    };
    (audio,publications) => {
        "admin_transcription_price_publications"
    };
    (text,publications) => {
        "admin_text_price_publications"
    };
    (audio,catalog) => {
        "transcription_prices"
    };
    (text,catalog) => {
        "text_prices"
    };
    ($family:ident,$literal:literal) => {
        $literal
    };
}
macro_rules! catalog_sql{
    ($audio:expr; $($part:tt),+) => {if $audio {concat!($(catalog_part!(audio,$part)),+)}else{concat!($(catalog_part!(text,$part)),+)}};
}
fn is_audio(price: &str) -> bool {
    price.starts_with("asr-price-")
}
async fn draft_price_id(tx: &mut Tx<'_>, profile: &str, audio: bool) -> Result<String, AdminError> {
    let fixed:Option<Value>=sqlx::query_scalar("SELECT s.manifest->'pricing' FROM registry_model_states s JOIN reviewed_execution_profiles p USING(model_version_id)
        WHERE p.execution_profile_id=$1").bind(profile).fetch_optional(&mut **tx).await?;
    let prefix = if audio { "asr-price-" } else { "price-" };
    if let Some(pricing) = fixed {
        let id = pricing["price_version"]
            .as_str()
            .filter(|id| pricing["policy"] == "price_version" && id.starts_with(prefix))
            .ok_or(AdminError::Conflict)?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM admin_text_price_drafts WHERE price_version=$1 UNION ALL SELECT FROM admin_transcription_price_drafts WHERE price_version=$1
            UNION ALL SELECT FROM text_prices WHERE price_version=$1 UNION ALL SELECT FROM transcription_prices WHERE price_version=$1)")
            .bind(id).fetch_one(&mut **tx).await?;
        if exists {
            return Err(AdminError::Conflict);
        }
        return Ok(id.to_owned());
    }
    Ok(format!("{prefix}{}", uuid::Uuid::new_v4()))
}
fn scope_of(row: &sqlx::postgres::PgRow) -> Result<Scope, AdminError> {
    Ok(Scope {
        environment: "dev".into(),
        kind: row.try_get("scope_kind")?,
        scope_id: row.try_get("scope_id")?,
    })
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    pub price_version_id: String,
    effective_at: String,
    review_report_id: String,
}
fn effective_seconds(value: &str) -> Result<i64, AdminError> {
    if !value.ends_with('Z') {
        return Err(AdminError::Invalid);
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| AdminError::Invalid)?;
    if parsed.nanosecond() != 0 || parsed.unix_timestamp() < 0 {
        return Err(AdminError::Invalid);
    }
    Ok(parsed.unix_timestamp())
}
impl Publish {
    pub(super) fn validate(&self, request: &ActionRequest) -> Result<(), AdminError> {
        valid_scope(&request.scope)?;
        if !id(&self.price_version_id)
            || !orange_protocol::valid_digest(&self.review_report_id)
            || !orange_protocol::valid_digest(&request.target_version)
        {
            return Err(AdminError::Invalid);
        }
        effective_seconds(&self.effective_at)?;
        Ok(())
    }
}
fn valid_scope(scope: &Scope) -> Result<(), AdminError> {
    if scope.environment != "dev"
        || !id(&scope.scope_id)
        || !matches!(scope.kind.as_str(), "platform" | "tenant")
        || (scope.kind == "platform" && scope.scope_id != "platform")
    {
        return Err(AdminError::Invalid);
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terms {
    scope: Scope,
    node_id: String,
    execution_profile_id: String,
    meter_profile_digest: String,
    input_price: Money,
    output_price: Money,
    output_limit: u32,
    fee_bps: u16,
}
impl Terms {
    fn validate(&self) -> Result<(), AdminError> {
        valid_scope(&self.scope)?;
        if !id(&self.node_id)
            || !id(&self.execution_profile_id)
            || !orange_protocol::valid_digest(&self.meter_profile_digest)
            || !(1..=4096).contains(&self.output_limit)
            || self.fee_bps > 10000
        {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
    cursor: Option<String>,
    limit: Option<u16>,
}
impl ReadQuery {
    fn scope(&self) -> Result<Scope, AdminError> {
        let scope = Scope {
            environment: self.environment.clone(),
            kind: self.scope_kind.clone(),
            scope_id: self.scope_id.clone(),
        };
        valid_scope(&scope)?;
        Ok(scope)
    }
}
fn terms(row: &sqlx::postgres::PgRow) -> Result<Terms, AdminError> {
    let units = |field| -> Result<Money, AdminError> {
        Ok(Money(
            orange_domain::Credits::new(row.try_get(field)?)
                .map_err(|_| AdminError::Unavailable)?,
        ))
    };
    Ok(Terms {
        scope: scope_of(row)?,
        node_id: row.try_get("node_id")?,
        execution_profile_id: row.try_get("execution_profile_id")?,
        meter_profile_digest: row.try_get("meter_profile_digest")?,
        input_price: units("input_price")?,
        output_price: units("output_price")?,
        output_limit: u32::try_from(row.try_get::<i32, _>("output_limit")?)
            .map_err(|_| AdminError::Unavailable)?,
        fee_bps: u16::try_from(row.try_get::<i32, _>("fee_bps")?)
            .map_err(|_| AdminError::Unavailable)?,
    })
}
async fn binding(tx: &mut Tx<'_>, t: &Terms) -> Result<(String, Value), AdminError> {
    t.validate()?;
    let row=sqlx::query("SELECT n.owner_id,r.model_version_id,r.model_sha256,r.execution_profile_digest,c.executable_sha256,c.context_tokens,m.descriptor
         FROM nodes n
         JOIN owners o USING(owner_id)
         CROSS JOIN reviewed_execution_profiles r
         JOIN reviewed_cpu_profiles c ON c.execution_profile_id=r.execution_profile_id
         CROSS JOIN trusted_meter_profiles m
         WHERE n.node_id=$1
           AND n.environment='dev'
           AND n.state!='revoked'
           AND o.state='active'
           AND ($2::text IS NULL OR n.owner_id=$2)
           AND r.execution_profile_id=$3
           AND r.enabled
           AND m.profile_digest=$4
           AND m.enabled
         FOR SHARE OF n,o,r,c,m")
        .bind(&t.node_id).bind((t.scope.kind=="tenant").then_some(&t.scope.scope_id)).bind(&t.execution_profile_id).bind(&t.meter_profile_digest).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    let descriptor: Value = row.try_get("descriptor")?;
    let execution: String = row.try_get("execution_profile_digest")?;
    let expected = digest(
        "orange.trusted-meter.v1",
        &json!({"execution_digest":execution,"input_rule":"llama-chat-template-input-v1","output_rule":orange_model_runtime::metering::PROFILE,"tool_schema_rule":orange_model_runtime::tool_schema::RULE}),
    )?;
    let ctx = descriptor["context_tokens"]
        .as_u64()
        .ok_or(AdminError::Conflict)?;
    if expected != t.meter_profile_digest
        || descriptor["profile_digest"] != expected
        || descriptor["execution_digest"] != execution
        || descriptor["model_version"] != row.try_get::<String, _>("model_version_id")?
        || descriptor["model_sha256"] != row.try_get::<String, _>("model_sha256")?
        || descriptor["executable_sha256"] != row.try_get::<String, _>("executable_sha256")?
        || ctx
            != u64::try_from(row.try_get::<i32, _>("context_tokens")?)
                .map_err(|_| AdminError::Conflict)?
        || descriptor["input_rule"] != "llama-chat-template-input-v1"
        || descriptor["output_rule"] != orange_model_runtime::metering::PROFILE
        || descriptor["tool_schema_rule"] != orange_model_runtime::tool_schema::RULE
        || !(256..=32768).contains(&ctx)
        || u64::from(t.output_limit) >= ctx
    {
        return Err(AdminError::Conflict);
    }
    TextQuote {
        input_units: ctx,
        output_limit: u64::from(t.output_limit),
        input_price: t.input_price.0,
        output_price: t.output_price.0,
    }
    .maximum()
    .map_err(|_| AdminError::Invalid)?;
    Ok((row.try_get("owner_id")?, descriptor))
}
fn project(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    let price = row.try_get::<&str, _>("price_version")?;
    let t = if is_audio(price) {
        json!(audio::terms(row)?)
    } else {
        json!(terms(row)?)
    };
    let mut value = json!({
        "price_version_id":row.try_get::<String,_>("price_version")?,
        "terms":t,
        "provider_owner_id":row.try_get::<String,_>("provider_owner_id")?,
        "target_version":row.try_get::<String,_>("terms_digest")?,
        "review_report_id":row.try_get::<String,_>("review_report_id")?,
        "created_by":row.try_get::<String,_>("created_by")?,
        "created_at":timestamp(row.try_get("created_at")?)?,
        "state":row.try_get::<Option<String>,_>("state")?.unwrap_or_else(||"draft".into())
    });
    if is_audio(price) {
        value["node_id"] = json!(row.try_get::<&str, _>("node_id")?);
        value["execution_profile_id"] = json!(row.try_get::<&str, _>("execution_profile_id")?);
    }
    if let Some(at) = row.try_get::<Option<i64>, _>("effective_at")? {
        value["effective_at"] = json!(timestamp(at)?);
    }
    if let Some(id) = row.try_get::<Option<String>, _>("action_id")? {
        value["publication_action_id"] = json!(id);
    }
    Ok(value)
}
async fn one(
    tx: &mut Tx<'_>,
    scope: &Scope,
    price: &str,
) -> Result<sqlx::postgres::PgRow, AdminError> {
    let audio = is_audio(price);
    sqlx::query(catalog_sql!(audio; "SELECT d.*,p.state,p.effective_at,p.action_id FROM ", drafts, " d LEFT JOIN ", publications, " p USING(price_version)
        WHERE d.price_version=$1 AND d.scope_kind=$2 AND d.scope_id=$3"))
    .bind(price)
    .bind(&scope.kind)
    .bind(&scope.scope_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AdminError::NotFound)
}
pub async fn create(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    body: Result<Json<Terms>, JsonRejection>,
) -> Result<Response, AdminError> {
    let t = body.map_err(|_| AdminError::Invalid)?.0;
    t.validate()?;
    let key = key(&headers)?;
    let mut tx = begin(&identity).await?;
    let session = authenticate(&identity, &headers, &mut tx).await?;
    let principal = &session.view.principal_id;
    let deadline = actor(&mut tx, principal, &t.scope, &["pricing.draft"]).await?;
    let request_digest = digest("admin-price-draft.v1", &t)?;
    let old: Option<(String, String)> = sqlx::query_as("SELECT price_version,request_digest FROM admin_text_price_drafts WHERE created_by=$1 AND request_key=$2").bind(principal).bind(key).fetch_optional(&mut *tx).await?;
    let price = if let Some((price, prior)) = old {
        if prior != request_digest {
            return Err(AdminError::Idempotency);
        }
        price
    } else {
        let (owner, descriptor) = binding(&mut tx, &t).await?;
        let price = draft_price_id(&mut tx, &t.execution_profile_id, false).await?;
        let hash = digest("admin-price-terms.v1", &(&price, &owner, &t))?;
        let report = digest("admin-price-review.v1", &(&hash, &descriptor))?;
        sqlx::query("INSERT INTO admin_text_price_drafts(price_version,created_by,request_key,request_digest,scope_kind,scope_id,provider_owner_id,node_id,execution_profile_id,meter_profile_digest,input_price,output_price,output_limit,fee_bps,terms_digest,review_report_id,created_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,FLOOR(EXTRACT(EPOCH
         FROM clock_timestamp()))::bigint)").bind(&price).bind(principal).bind(key).bind(request_digest).bind(&t.scope.kind).bind(&t.scope.scope_id).bind(owner).bind(&t.node_id).bind(&t.execution_profile_id).bind(&t.meter_profile_digest).bind(t.input_price.0.units()).bind(t.output_price.0.units()).bind(t.output_limit as i32).bind(i32::from(t.fee_bps)).bind(hash).bind(report).execute(&mut *tx).await?;
        audit(
            &mut tx,
            principal,
            &t.scope,
            "pricing.draft_created",
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
async fn audit(
    tx: &mut Tx<'_>,
    who: &str,
    scope: &Scope,
    kind: &str,
    entity: Option<&str>,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,FLOOR(EXTRACT(EPOCH
         FROM clock_timestamp()))::bigint)").bind(uuid::Uuid::new_v4().to_string()).bind(who).bind(kind).bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(entity).execute(&mut **tx).await?;
    Ok(())
}
pub async fn get_one<const AUDIO: bool>(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(price): Path<String>,
    query: Result<axum::extract::Query<ReadQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let q = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = q.scope()?;
    if q.limit.is_some() || q.cursor.is_some() || !id(&price) || AUDIO != is_audio(&price) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("pricing.read", &scope)?;
    let value = project(&one(&mut tx, &scope, &price).await?)?;
    audit(
        &mut tx,
        &session.view.principal_id,
        &scope,
        "pricing.draft_read",
        Some(&price),
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("pricing.read", &scope)?;
    tx.commit().await?;
    Ok(Json(value))
}
pub async fn list<const AUDIO: bool>(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<ReadQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let q = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = q.scope()?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("pricing.read", &scope)?;
    let audio = AUDIO;
    let fingerprint = digest(
        if AUDIO {
            "admin-transcription-price-pages.v1"
        } else {
            "admin-price-pages.v1"
        },
        &(
            &scope,
            &session.view.principal_id,
            &session.view.policy_version,
            session.grants.get(&scope),
        ),
    )?;
    let session_hash = hash_secret(&cookie(&headers, SESSION_COOKIE)?);
    let (upper, before) = if let Some(token) = q.cursor {
        if token.is_empty() || token.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let (u, b): (i64, i64) = sqlx::query_as(
            "SELECT snapshot_sequence,before_sequence
         FROM admin_audit_cursors
         WHERE cursor_hash=$1
           AND session_hash=$2
           AND authority=$3
           AND principal_id=$4
           AND expires_at>FLOOR(EXTRACT(EPOCH
         FROM clock_timestamp()))::bigint",
        )
        .bind(hash_secret(&token))
        .bind(&session_hash)
        .bind(&fingerprint)
        .bind(&session.view.principal_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AdminError::Invalid)?;
        (u, Some(b))
    } else {
        (sqlx::query_scalar::<_,i64>(catalog_sql!(audio; "SELECT coalesce(max(draft_sequence),0) FROM ", drafts, " WHERE scope_kind=$1 AND scope_id=$2")).bind(&scope.kind).bind(&scope.scope_id).fetch_one(&mut *tx).await?,None)
    };
    let rows = sqlx::query(
        catalog_sql!(audio; "SELECT d.*,p.state,p.effective_at,p.action_id
         FROM ", drafts, " d
         LEFT JOIN ", publications, " p USING(price_version)
         WHERE d.scope_kind=$1
           AND d.scope_id=$2
           AND draft_sequence<=$3
           AND ($4::bigint IS NULL OR draft_sequence<$4)
         ORDER BY draft_sequence DESC
         LIMIT $5"),
    )
    .bind(&scope.kind)
    .bind(&scope.scope_id)
    .bind(upper)
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *tx)
    .await?;
    let visible = &rows[..rows.len().min(usize::from(limit))];
    let mut value = json!({"data":visible.iter().map(project).collect::<Result<Vec<_>,_>>()?});
    if rows.len() > visible.len() {
        let last = visible
            .last()
            .ok_or(AdminError::Unavailable)?
            .try_get("draft_sequence")?;
        value["next_cursor"] = json!(
            crate::admin::routes::audit::next_cursor(
                &mut tx,
                &session_hash,
                &session,
                &fingerprint,
                upper,
                last
            )
            .await?
        );
    }
    audit(
        &mut tx,
        &session.view.principal_id,
        &scope,
        "pricing.drafts_listed",
        None,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("pricing.read", &scope)?;
    tx.commit().await?;
    Ok(Json(value))
}
pub(super) async fn target(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Publish,
) -> Result<(i64, String), AdminError> {
    let row = one(tx, &r.scope, &p.price_version_id).await?;

    if row.try_get::<String, _>("terms_digest")? != r.target_version
        || row.try_get::<String, _>("review_report_id")? != p.review_report_id
    {
        return Err(AdminError::Precondition);
    }
    if row.try_get::<Option<String>, _>("state")?.is_some() {
        return Err(AdminError::Conflict);
    }
    let audio = is_audio(&p.price_version_id);

    let occupied: bool = sqlx::query_scalar(
        catalog_sql!(audio; "SELECT EXISTS(SELECT 1 FROM ", catalog, " WHERE price_version=$1)"),
    )
    .bind(&p.price_version_id)
    .fetch_one(&mut **tx)
    .await?;
    if occupied {
        return Err(AdminError::Conflict);
    }
    if effective_seconds(&p.effective_at)? < row.try_get::<i64, _>("created_at")? {
        return Err(AdminError::Invalid);
    }
    if audio {
        audio::verify(tx, &p.price_version_id, &row).await?;
        return Ok((1, "draft".into()));
    }
    let t = terms(&row)?;
    let (owner, descriptor) = binding(tx, &t).await?;
    if digest("admin-price-terms.v1", &(&p.price_version_id, owner, &t))? != r.target_version
        || digest("admin-price-review.v1", &(&r.target_version, descriptor))? != p.review_report_id
    {
        return Err(AdminError::Conflict);
    }
    Ok((1, "draft".into()))
}
pub(super) async fn require_creator(
    tx: &mut Tx<'_>,
    p: &Publish,
    principal: &str,
) -> Result<(), AdminError> {
    let creator: String = sqlx::query_scalar(catalog_sql!(is_audio(&p.price_version_id); "SELECT created_by FROM ", drafts, " WHERE price_version=$1"))
    .bind(&p.price_version_id)
    .fetch_one(&mut **tx)
    .await?;
    if creator != principal {
        return Err(AdminError::Forbidden);
    }
    Ok(())
}
pub(super) async fn publish(
    tx: &mut Tx<'_>,
    p: &Publish,
    action: &str,
    checker: &str,
    executor: &str,
) -> Result<(), AdminError> {
    let audio = is_audio(&p.price_version_id);

    sqlx::query(catalog_sql!(audio; "INSERT INTO ", publications, "(price_version,action_id,effective_at,checker_id,executor_id,state) VALUES($1,$2,$3,$4,$5,'scheduled')")).bind(&p.price_version_id).bind(action).bind(effective_seconds(&p.effective_at)?).bind(checker).bind(executor).execute(&mut **tx).await?;
    if activate_one(tx, audio, Some(&p.price_version_id)).await? {
        let state: String = sqlx::query_scalar(
            catalog_sql!(audio; "SELECT state FROM ", publications, " WHERE price_version=$1"),
        )
        .bind(&p.price_version_id)
        .fetch_one(&mut **tx)
        .await?;
        if state != "active" {
            return Err(AdminError::Conflict);
        }
    }
    Ok(())
}
async fn activation_deadline(
    tx: &mut Tx<'_>,
    row: &sqlx::postgres::PgRow,
    scope: &Scope,
) -> Result<i64, AdminError> {
    let maker: &str = row.try_get("created_by")?;
    let checker: &str = row.try_get("checker_id")?;
    if maker == checker {
        return Err(AdminError::Forbidden);
    }
    let maker_until = actor(tx, maker, scope, &["actions.create", "pricing.publish"]).await?;
    let checker_until = actor(
        tx,
        checker,
        scope,
        &["actions.approve", "pricing.publish.approve"],
    )
    .await?;
    let executor_until = actor(
        tx,
        row.try_get("executor_id")?,
        scope,
        &["actions.execute", "pricing.publish"],
    )
    .await?;
    Ok(maker_until.min(checker_until).min(executor_until))
}
/// The scheduler and immediate execution use the same publication lock. An
/// admission failure is terminal for this version. Database failures roll back
/// the transaction, leaving a scheduled row available for the next run.
async fn materialize(
    tx: &mut Tx<'_>,
    row: &sqlx::postgres::PgRow,
    price: &str,
    t: &Terms,
) -> Result<bool, AdminError> {
    let deadline = activation_deadline(tx, row, &t.scope).await?;
    let (owner, descriptor) = binding(tx, t).await?;
    let hash = digest("admin-price-terms.v1", &(price, owner, t))?;
    if hash != row.try_get::<String, _>("terms_digest")?
        || digest("admin-price-review.v1", &(&hash, descriptor))?
            != row.try_get::<String, _>("review_report_id")?
    {
        return Err(AdminError::Conflict);
    }
    let inserted = sqlx::query(
        "INSERT INTO text_prices
           (price_version,node_id,execution_profile_id,meter_profile_digest,input_price,output_price,output_limit,fee_bps)
         SELECT $1,$2,$3,$4,$5,$6,$7,$8 WHERE $9 > FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint
         ON CONFLICT(price_version) DO NOTHING")
        .bind(price).bind(&t.node_id).bind(&t.execution_profile_id).bind(&t.meter_profile_digest)
        .bind(t.input_price.0.units()).bind(t.output_price.0.units()).bind(i64::from(t.output_limit))
        .bind(i32::from(t.fee_bps)).bind(deadline).execute(&mut **tx).await?;
    Ok(inserted.rows_affected() == 1)
}

async fn activate_one(
    tx: &mut Tx<'_>,
    audio: bool,
    only: Option<&str>,
) -> Result<bool, AdminError> {
    let row = sqlx::query(
        catalog_sql!(audio; "SELECT d.*,p.checker_id,p.executor_id,p.action_id
         FROM ", publications, " p JOIN ", drafts, " d USING(price_version)
         WHERE p.state='scheduled' AND effective_at<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint
           AND ($1::text IS NULL OR p.price_version=$1)
         ORDER BY effective_at,price_version LIMIT 1 FOR UPDATE OF p SKIP LOCKED"))
        .bind(only).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let price: String = row.try_get("price_version")?;
    let scope = scope_of(&row)?;
    let result = if audio {
        audio::activate(tx, &row, &price, &scope).await
    } else {
        materialize(tx, &row, &price, &terms(&row)?).await
    };
    let (state, event) = match result {
        Ok(true) => ("active", "pricing.publication_activated"),
        Ok(false) => ("blocked", "pricing.publication_not_inserted"),
        Err(AdminError::Forbidden) => ("blocked", "pricing.publication_authority_lost"),
        Err(AdminError::Conflict | AdminError::Invalid | AdminError::NotFound) => {
            ("blocked", "pricing.publication_admission_lost")
        }
        Err(error) => return Err(error),
    };
    sqlx::query(
        catalog_sql!(audio; "UPDATE ", publications, " SET state=$2,completed_at=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint
         WHERE price_version=$1 AND state='scheduled'"))
        .bind(&price).bind(state).execute(&mut **tx).await?;
    audit(
        tx,
        "system.price-scheduler",
        &scope,
        event,
        Some(row.try_get("action_id")?),
    )
    .await?;
    Ok(true)
}

pub async fn activate_due(pool: &sqlx::PgPool) -> Result<usize, AdminError> {
    let mut count = 0;
    while count < 100 {
        let mut tx = pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='2s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout='5s'")
            .execute(&mut *tx)
            .await?;
        let first = count % 2 == 1;
        let handled = activate_one(&mut tx, first, None).await?
            || activate_one(&mut tx, !first, None).await?;

        tx.commit().await?;
        if !handled {
            break;
        }
        count += 1;
    }
    Ok(count)
}
