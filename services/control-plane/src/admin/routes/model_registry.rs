//! A registered draft is immutable metadata. Only the trusted profile authority can admit execution.
use super::*;
use crate::admin::actions;
use axum::{body::Bytes, extract::Path};
use orange_model_runtime::manifest::ValidatedManifest;
use serde::Deserialize;

const READ: &str = "models.read";
const DRAFT: &str = "models.draft";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    scope: Scope,
    manifest: Value,
}

fn platform(scope: &Scope) -> Result<(), AdminError> {
    if scope.environment == "dev" && scope.kind == "platform" && scope.scope_id == "platform" {
        Ok(())
    } else {
        Err(AdminError::Forbidden)
    }
}
fn summary_value(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    Ok(
        json!({"model_version_id":row.try_get::<&str,_>("model_version_id")?,"version":row.try_get::<&str,_>("manifest_sha256")?,
        "modality":row.try_get::<&str,_>("modality")?,"status":"draft"}),
    )
}
fn receipt(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    let manifest: Value = row.try_get("manifest")?;
    let version =
        ValidatedManifest::parse(&manifest.to_string()).map_err(|_| AdminError::Unavailable)?;
    let hash: &str = row.try_get("manifest_sha256")?;
    if version.receipt().manifest_sha256 != hash
        || version.receipt().manifest.model_version != row.try_get::<&str, _>("model_version_id")?
    {
        return Err(AdminError::Unavailable);
    }
    let principal: &str = row.try_get("created_by")?;
    let key: &str = row.try_get("request_key")?;
    let registration = orange_protocol::request_digest(
        "admin-model-draft-registration.v1",
        &(principal, key, hash),
    )
    .map_err(|_| AdminError::Unavailable)?;
    Ok(
        json!({"model":summary_value(row)?,"manifest":manifest,"manifest_sha256":hash,"registered_by":principal,"registration_sha256":registration}),
    )
}
async fn one(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    version: &str,
) -> Result<sqlx::postgres::PgRow, AdminError> {
    sqlx::query("SELECT model_version_id,manifest_sha256,created_by,request_key,manifest,manifest#>>'{interface,modality}' AS modality
       FROM admin_model_drafts WHERE model_version_id=$1").bind(version).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)
}
pub(super) async fn create(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AdminError> {
    let mime = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next());
    if !mime.is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        || headers.get_all("idempotency-key").iter().count() != 1
    {
        return Err(AdminError::Invalid);
    }
    let raw = std::str::from_utf8(&body).map_err(|_| AdminError::Invalid)?;
    let value =
        orange_protocol::canonical::parse_tool_arguments(raw).map_err(|_| AdminError::Invalid)?;
    let prop: Proposal = serde_json::from_value(value).map_err(|_| AdminError::Invalid)?;
    platform(&prop.scope)?;
    let version =
        ValidatedManifest::parse(&prop.manifest.to_string()).map_err(|_| AdminError::Invalid)?;
    let model = &version.receipt().manifest.model_version;
    let sha = &version.receipt().manifest_sha256;
    let key = actions::key(&headers)?;
    let req = audit::request_id(&headers)?;
    let mut tx = actions::begin(&identity).await?;
    let current = actions::authenticate(&identity, &headers, &mut tx).await?;
    let who = &current.view.principal_id;
    let deadline = actions::actor(&mut tx, who, &prop.scope, &[READ, DRAFT]).await?;
    let prior:Option<(String,String)>=sqlx::query_as("SELECT model_version_id,manifest_sha256 FROM admin_model_drafts WHERE created_by=$1 AND request_key=$2")
        .bind(who).bind(key).fetch_optional(&mut *tx).await?;
    let status = if let Some((old, hash)) = prior {
        if old != *model || hash != *sha {
            return Err(AdminError::Idempotency);
        }
        StatusCode::OK
    } else {
        let result=sqlx::query("INSERT INTO admin_model_drafts(model_version_id,manifest_sha256,manifest,created_by,request_key) VALUES($1,$2,$3,$4,$5)")
            .bind(model).bind(sha).bind(&prop.manifest).bind(who).bind(key).execute(&mut *tx).await;
        if let Err(error) = result {
            if error.as_database_error().is_some_and(|db| {
                db.is_unique_violation()
                    || db.constraint() == Some("model_version_namespace_conflict")
            }) {
                return Err(AdminError::Conflict);
            }
            return Err(error.into());
        }
        audit::record_event(
            &mut tx,
            &current,
            &prop.scope,
            "model.draft_registered",
            Some(model),
            &req,
        )
        .await?;
        StatusCode::CREATED
    };
    let projected = receipt(&one(&mut tx, model).await?)?;
    actions::finish(&identity, &headers, &mut tx, deadline).await?;
    tx.commit().await?;
    let mut response = audit::response(projected, &req)?;
    *response.status_mut() = status;
    Ok(response)
}

fn cursor(current: &CurrentSession, scope: &Scope, version: &str) -> Result<String, AdminError> {
    let grants = current.grants.get(scope).ok_or(AdminError::Forbidden)?;
    let digest = orange_protocol::request_digest(
        "model-registry-list.v1",
        &(
            &current.view.principal_id,
            &current.view.policy_version,
            grants,
            scope,
            version,
        ),
    )
    .map_err(|_| AdminError::Unavailable)?;
    Ok(format!("{version}~{digest}"))
}

pub(super) async fn list(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let q = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = q.scope()?;
    platform(&scope)?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let req = audit::request_id(&headers)?;
    let mut tx = identity.pool.begin().await?;
    let current = identity.current_in(&headers, &mut tx).await?;
    current.authorize(READ, &scope)?;
    let after = match &q.cursor {
        None => "",
        Some(c) => {
            let (id, _) = c.split_once('~').ok_or(AdminError::Invalid)?;
            if !orange_protocol::valid_id(id) || cursor(&current, &scope, id)? != *c {
                return Err(AdminError::Invalid);
            }
            id
        }
    };
    let rows=sqlx::query("SELECT model_version_id,manifest_sha256,manifest#>>'{interface,modality}' AS modality
        FROM admin_model_drafts WHERE model_version_id COLLATE \"C\">$1 ORDER BY model_version_id COLLATE \"C\" LIMIT $2")
        .bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let page = rows
        .iter()
        .take(usize::from(limit))
        .map(summary_value)
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = json!({"data":page});
    if rows.len() > usize::from(limit) {
        result["next_cursor"] = json!(cursor(
            &current,
            &scope,
            rows[usize::from(limit) - 1].try_get("model_version_id")?
        )?);
    }
    audit::record_read(
        &mut tx,
        &current,
        &scope,
        "model.directory_read",
        None,
        &req,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize(READ, &scope)?;
    tx.commit().await?;
    audit::response(result, &req)
}

async fn read_one(
    identity: Arc<AdminIdentity>,
    headers: HeaderMap,
    id: String,
    q: ScopedQuery,
    full: bool,
) -> Result<Response, AdminError> {
    let scope = q.scope()?;
    platform(&scope)?;
    if !orange_protocol::valid_id(&id) || q.limit.is_some() || q.cursor.is_some() {
        return Err(AdminError::Invalid);
    }
    let req = audit::request_id(&headers)?;
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize(READ, &scope)?;
    let row = one(&mut tx, &id).await?;
    let result = if full {
        receipt(&row)?
    } else {
        summary_value(&row)?
    };
    audit::record_read(
        &mut tx,
        &session,
        &scope,
        "model.draft_read",
        Some(&id),
        &req,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize(READ, &scope)?;
    tx.commit().await?;
    audit::response(result, &req)
}
pub(super) async fn summary(
    State(i): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    q: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    read_one(i, headers, id, q.map_err(|_| AdminError::Invalid)?.0, false).await
}
pub(super) async fn detail(
    State(i): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    q: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    read_one(i, headers, id, q.map_err(|_| AdminError::Invalid)?.0, true).await
}
