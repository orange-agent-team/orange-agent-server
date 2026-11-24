//! Model activation consults independently verified certification on proposal, approval and execution.
use super::super::routes::audit as events;
use super::*;
use crate::admin::model_review::Envelope;
use axum::extract::{Query, rejection::QueryRejection};
use orange_model_runtime::manifest::ValidatedManifest;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub model_version_id: String,
    pub certification_report_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disable {
    pub model_version_id: String,
}
pub(super) fn scope(s: &Scope) -> Result<(), AdminError> {
    if s.environment == "dev" && s.kind == "platform" && s.scope_id == "platform" {
        Ok(())
    } else {
        Err(AdminError::Forbidden)
    }
}
pub(super) fn validate(
    r: &ActionRequest,
    model: &str,
    report: Option<&str>,
) -> Result<(), AdminError> {
    scope(&r.scope)?;
    if !id(model)
        || !id(&r.target_version)
        || !matches!(
            (r.action_type.as_str(), report),
            ("model.activate", Some(_)) | ("model.disable", None)
        )
        || report.is_some_and(|v| !orange_protocol::valid_digest(v))
    {
        return Err(AdminError::Invalid);
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    scope: Scope,
    certification: Envelope,
}
async fn manifest(tx: &mut Tx<'_>, id: &str) -> Result<ValidatedManifest, AdminError> {
    let stored=sqlx::query("SELECT manifest,manifest_sha256 FROM admin_model_drafts WHERE model_version_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    let json: Value = stored.try_get("manifest")?;
    let parsed =
        ValidatedManifest::parse(&json.to_string()).map_err(|_| AdminError::Unavailable)?;
    if parsed.receipt().manifest_sha256 != stored.try_get::<&str, _>("manifest_sha256")? {
        return Err(AdminError::Unavailable);
    }
    Ok(parsed)
}
fn project(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    Ok(
        json!({"report_id":row.try_get::<&str,_>("report_id")?,"model_version_id":row.try_get::<&str,_>("model_version_id")?,
        "manifest_sha256":row.try_get::<&str,_>("manifest_sha256")?,"signing_key_sha256":row.try_get::<&str,_>("signing_key_sha256")?,
        "execution_profile_id":row.try_get::<&str,_>("execution_profile_id")?,"execution_profile_digest":row.try_get::<&str,_>("execution_profile_digest")?,
        "model_sha256":row.try_get::<&str,_>("model_sha256")?,"issued_unix":row.try_get::<i64,_>("issued_unix")?.to_string(),
        "expires_unix":row.try_get::<i64,_>("expires_unix")?.to_string(),"registered_by":row.try_get::<&str,_>("registered_by")?}),
    )
}
pub async fn register(
    State(identity): State<Arc<AdminIdentity>>,
    Path(model): Path<String>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Result<Response, AdminError> {
    if headers.get_all("idempotency-key").iter().count() != 1
        || !headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(AdminError::Invalid);
    }
    let value = orange_protocol::canonical::parse_tool_arguments(
        std::str::from_utf8(&bytes).map_err(|_| AdminError::Invalid)?,
    )
    .map_err(|_| AdminError::Invalid)?;
    let sub: Submission = serde_json::from_value(value).map_err(|_| AdminError::Invalid)?;
    scope(&sub.scope)?;
    if sub.certification.report.model_version_id != model {
        return Err(AdminError::Invalid);
    }
    let req = events::request_id(&headers)?;
    let request_key = key(&headers)?;
    let mut tx = begin(&identity).await?;
    let session = authenticate(&identity, &headers, &mut tx).await?;
    let who = &session.view.principal_id;
    let deadline = actor(&mut tx, who, &sub.scope, &["models.read", "models.draft"]).await?;
    let trust = identity
        .review_trust
        .as_ref()
        .ok_or(AdminError::Unavailable)?;
    let m = manifest(&mut tx, &model).await?;
    let (report, issued, expires) =
        sub.certification
            .check(trust, &m.receipt().manifest, now(&mut tx).await?)?;
    let r = &sub.certification.report;
    let prior: Option<String> = sqlx::query_scalar(
        "SELECT report_id FROM model_certifications WHERE registered_by=$1 AND request_key=$2",
    )
    .bind(who)
    .bind(request_key)
    .fetch_optional(&mut *tx)
    .await?;
    let status = if let Some(exists) = prior {
        if exists != report {
            return Err(AdminError::Idempotency);
        }
        StatusCode::OK
    } else {
        let state: String = sqlx::query_scalar(
            "SELECT status FROM registry_model_states WHERE model_version_id=$1",
        )
        .bind(&model)
        .fetch_one(&mut *tx)
        .await?;
        if state != "draft" {
            return Err(AdminError::Conflict);
        }
        let stored = serde_json::to_value(&sub.certification).map_err(|_| AdminError::Invalid)?;
        let result=sqlx::query("INSERT INTO model_certifications(report_id,model_version_id,manifest_sha256,signing_key_sha256,model_sha256,execution_profile_id,
            execution_profile_digest,issued_unix,expires_unix,envelope,registered_by,request_key)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT DO NOTHING")
            .bind(&report).bind(&model).bind(&r.manifest_sha256).bind(&trust.fingerprint).bind(&r.model_sha256).bind(&r.execution_profile_id)
            .bind(&r.execution_profile_digest).bind(issued).bind(expires).bind(stored).bind(who).bind(request_key).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(AdminError::Conflict);
        }
        events::record_event(
            &mut tx,
            &session,
            &sub.scope,
            "model.certification_registered",
            Some(&report),
            &req,
        )
        .await?;
        StatusCode::CREATED
    };
    let data = project(
        &sqlx::query("SELECT * FROM model_certifications WHERE report_id=$1")
            .bind(&report)
            .fetch_one(&mut *tx)
            .await?,
    )?;
    finish(&identity, &headers, &mut tx, deadline.min(expires)).await?;
    tx.commit().await?;
    let mut response = events::response(data, &req)?;
    *response.status_mut() = status;
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportQuery {
    cursor: Option<String>,
    limit: Option<u16>,
    environment: String,
    scope_kind: String,
    scope_id: String,
}
pub async fn read_report(
    State(identity): State<Arc<AdminIdentity>>,
    Path((model, report)): Path<(String, String)>,
    headers: HeaderMap,
    q: Result<Query<ReportQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    if q.limit.is_some() || q.cursor.is_some() {
        return Err(AdminError::Invalid);
    }
    let s = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    scope(&s)?;
    if !id(&model) || !orange_protocol::valid_digest(&report) {
        return Err(AdminError::Invalid);
    }
    let req = events::request_id(&headers)?;
    let mut tx = identity.pool.begin().await?;
    let who = identity.current_in(&headers, &mut tx).await?;
    who.authorize("models.read", &s)?;
    let row = sqlx::query(
        "SELECT * FROM model_certifications WHERE report_id=$1 AND model_version_id=$2",
    )
    .bind(&report)
    .bind(&model)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AdminError::NotFound)?;
    let mut data = project(&row)?;
    let env: Value = row.try_get("envelope")?;
    data["signature"] = env
        .get("signature")
        .cloned()
        .ok_or(AdminError::Unavailable)?;
    data["report"] = env.get("report").cloned().ok_or(AdminError::Unavailable)?;
    data["license_notice"] = env
        .get("license_notice")
        .cloned()
        .ok_or(AdminError::Unavailable)?;
    events::record_event(
        &mut tx,
        &who,
        &s,
        "model.certification_read",
        Some(&report),
        &req,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("models.read", &s)?;
    tx.commit().await?;
    events::response(data, &req)
}

pub(super) async fn target(
    tx: &mut Tx<'_>,
    request: &ActionRequest,
    model: &str,
    report: Option<&str>,
    identity: &AdminIdentity,
) -> Result<(i64, String), AdminError> {
    let version = manifest(tx, model).await?;
    let row =
        sqlx::query("SELECT status,version FROM registry_model_states WHERE model_version_id=$1")
            .bind(model)
            .fetch_one(&mut **tx)
            .await?;
    let state: String = row.try_get("status")?;
    if row.try_get::<&str, _>("version")? != request.target_version
        || state == "disabled"
        || (report.is_some() && state != "draft")
    {
        return Err(AdminError::Conflict);
    }
    if let Some(id) = report {
        let trust = identity
            .review_trust
            .as_ref()
            .ok_or(AdminError::Unavailable)?;
        let data: Value = sqlx::query_scalar(
            "SELECT envelope FROM model_certifications WHERE report_id=$1 AND model_version_id=$2",
        )
        .bind(id)
        .bind(model)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AdminError::NotFound)?;
        let cert: Envelope = serde_json::from_value(data).map_err(|_| AdminError::Unavailable)?;
        let (digest, _, _) = cert.check(trust, &version.receipt().manifest, now(tx).await?)?;
        if digest != id {
            return Err(AdminError::Conflict);
        }
    }
    Ok((if state == "draft" { 1 } else { 2 }, state))
}
pub(super) async fn activate(
    tx: &mut Tx<'_>,
    p: &Activation,
    action: &str,
    maker: &str,
    checker: &str,
) -> Result<i64, AdminError> {
    sqlx::query("INSERT INTO model_registry_activations(model_version_id,report_id,action_id,maker_id,checker_id) VALUES($1,$2,$3,$4,$5)")
        .bind(&p.model_version_id).bind(&p.certification_report_id).bind(action).bind(maker).bind(checker).execute(&mut **tx).await?;
    Ok(
        sqlx::query_scalar("SELECT expires_unix FROM model_certifications WHERE report_id=$1")
            .bind(&p.certification_report_id)
            .fetch_one(&mut **tx)
            .await?,
    )
}
pub(super) async fn disable(
    tx: &mut Tx<'_>,
    p: &Disable,
    action: &str,
    principal: &str,
) -> Result<(), AdminError> {
    sqlx::query(
        "INSERT INTO model_registry_disables(model_version_id,action_id,actor_id) VALUES($1,$2,$3)",
    )
    .bind(&p.model_version_id)
    .bind(action)
    .bind(principal)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn list_reports(
    State(identity): State<Arc<AdminIdentity>>,
    Path(model): Path<String>,
    headers: HeaderMap,
    q: Result<Query<ReportQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    let s = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    scope(&s)?;
    let limit = q.limit.unwrap_or(20);
    if !id(&model) || !(1..=100).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let req = events::request_id(&headers)?;
    let mut tx = identity.pool.begin().await?;
    let current = identity.current_in(&headers, &mut tx).await?;
    current.authorize("models.read", &s)?;
    let tag = |id: &str| {
        orange_protocol::request_digest(
            "model-certification-page.v1",
            &(
                &current.view.principal_id,
                &current.view.policy_version,
                &s,
                &current.grants.get(&s),
                &model,
                id,
            ),
        )
        .map_err(|_| AdminError::Unavailable)
    };
    let after = if let Some(value) = &q.cursor {
        let (id, token) = value.split_once('~').ok_or(AdminError::Invalid)?;
        if !orange_protocol::valid_digest(id) || tag(id)? != token {
            return Err(AdminError::Invalid);
        }
        id
    } else {
        ""
    };
    if !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT FROM admin_model_drafts WHERE model_version_id=$1)",
    )
    .bind(&model)
    .fetch_one(&mut *tx)
    .await?
    {
        return Err(AdminError::NotFound);
    }
    let rows=sqlx::query("SELECT report_id,model_version_id,manifest_sha256,signing_key_sha256,model_sha256,execution_profile_id,execution_profile_digest,issued_unix,expires_unix,registered_by
        FROM model_certifications WHERE model_version_id=$1 AND report_id>$2 ORDER BY report_id LIMIT $3")
        .bind(&model).bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let mut data = json!({"data":rows.iter().take(usize::from(limit)).map(project).collect::<Result<Vec<_>,_>>()?});
    if rows.len() > usize::from(limit) {
        let last = rows[usize::from(limit) - 1].try_get::<&str, _>("report_id")?;
        data["next_cursor"] = json!(format!("{}~{}", last, tag(last)?));
    }
    events::record_event(
        &mut tx,
        &current,
        &s,
        "model.certifications_listed",
        Some(&model),
        &req,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("models.read", &s)?;
    tx.commit().await?;
    events::response(data, &req)
}
