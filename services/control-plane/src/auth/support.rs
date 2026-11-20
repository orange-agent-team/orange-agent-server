//! User-originated access to explicit immutable personal artifact versions.
//! Operator approval and all later content reads must independently revalidate consent.
use super::*;
use crate::auth::users::UserSessionGuard;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use std::collections::BTreeSet;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    artifact_id: String,
    version_id: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    ticket_id: String,
    purpose: String,
    resources: Vec<Resource>,
    content_field: String,
    duration_seconds: u32,
}
impl Request {
    fn validate(&self) -> Result<(), AuthError> {
        let mut ids = BTreeSet::new();
        if !valid_id(&self.ticket_id)
            || self.purpose.trim().is_empty()
            || self.purpose.chars().count() > 500
            || self.content_field != "file_content"
            || !(1..=86400).contains(&self.duration_seconds)
            || !(1..=20).contains(&self.resources.len())
            || self.resources.iter().any(|r| {
                !valid_id(&r.artifact_id) || !valid_id(&r.version_id) || !ids.insert(&r.artifact_id)
            })
        {
            return Err(AuthError::Invalid);
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    before: Option<String>,
    limit: Option<u16>,
}
fn missing() -> UserApiError {
    UserApiError(StatusCode::NOT_FOUND, "NOT_FOUND")
}
fn db(_: sqlx::Error) -> UserApiError {
    AuthError::Unavailable.into()
}

macro_rules! consent_query { ($where:literal) => { concat!("SELECT c.authorization_id,c.user_id,c.owner_id,c.ticket_id,c.purpose,c.resource_versions,c.content_field,
 c.created_at,c.expires_at,r.revoked_at,
 CASE WHEN r.revoked_at IS NOT NULL THEN 'revoked'
      WHEN c.expires_at<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint THEN 'expired'
      WHEN EXISTS (SELECT 1 FROM jsonb_array_elements(c.resource_versions) v
                   WHERE NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.artifact_id=v->>'artifact_id'
                     AND a.version_id=v->>'version_id' AND a.owner_id=c.owner_id AND a.creator_id=c.user_id
                     AND a.resource_id IS NULL AND a.state='available' AND a.media_type='text/plain'))
           THEN 'unavailable' ELSE 'active' END AS status
 FROM personal_support_consents c LEFT JOIN personal_support_revocations r USING(authorization_id)
 WHERE c.user_id=$1 AND c.owner_id=$2", $where) }; }

fn view(row: PgRow) -> Result<Value, UserApiError> {
    let mut result = json!({
        "authorization_id":row.try_get::<String,_>("authorization_id").map_err(db)?,
        "subject_user_id":row.try_get::<String,_>("user_id").map_err(db)?,
        "owner_id":row.try_get::<String,_>("owner_id").map_err(db)?,
        "ticket_id":row.try_get::<String,_>("ticket_id").map_err(db)?,
        "purpose":row.try_get::<String,_>("purpose").map_err(db)?,
        "resources":row.try_get::<Value,_>("resource_versions").map_err(db)?,
        "content_field":row.try_get::<String,_>("content_field").map_err(db)?,
        "created_at":timestamp(row.try_get("created_at").map_err(db)?)?,
        "expires_at":timestamp(row.try_get("expires_at").map_err(db)?)?,
        "status":row.try_get::<String,_>("status").map_err(db)?,
    });
    if let Some(at) = row.try_get::<Option<i64>, _>("revoked_at").map_err(db)? {
        result["revoked_at"] = json!(timestamp(at)?);
    }
    Ok(result)
}
async fn one(guard: &mut UserSessionGuard, id: &str) -> Result<Value, UserApiError> {
    if !valid_id(id) {
        return Err(AuthError::Invalid.into());
    }
    let row = sqlx::query(consent_query!(" AND c.authorization_id=$3"))
        .bind(guard.view().user_id.clone())
        .bind(guard.view().personal_owner_id.clone())
        .bind(id)
        .fetch_optional(&mut **guard.transaction())
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
    view(row)
}
pub(super) async fn create(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    body: Result<Json<Request>, JsonRejection>,
) -> Result<Response, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    let key = headers
        .get("idempotency-key")
        .and_then(|k| k.to_str().ok())
        .filter(|k| valid_id(k))
        .ok_or(AuthError::Invalid)?;
    let Json(mut terms) = body.map_err(|_| AuthError::Invalid)?;
    terms.validate()?;
    terms
        .resources
        .sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
    let digest = orange_protocol::request_digest("personal.support-consent.v1", &terms)
        .map_err(|_| AuthError::Invalid)?;
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let tx = guard.transaction();
    // Serialize retries for the same user/key even when several sessions are active.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,3634943))")
        .bind(format!("{}:{}:{}", user.len(), user, key))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let prior:Option<(String,String)>=sqlx::query_as(
        "SELECT authorization_id,request_digest FROM personal_support_consents WHERE user_id=$1 AND request_key=$2")
        .bind(&user).bind(key).fetch_optional(&mut **tx).await.map_err(db)?;
    let id = if let Some((id, previous)) = prior {
        if previous != digest {
            return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
        }
        id
    } else {
        for resource in &terms.resources {
            let found=sqlx::query("SELECT artifact_id FROM artifacts WHERE artifact_id=$1 AND version_id=$2 AND owner_id=$3
                AND creator_id=$4 AND resource_id IS NULL AND state='available' AND media_type='text/plain' FOR SHARE")
                .bind(&resource.artifact_id).bind(&resource.version_id).bind(&owner).bind(&user)
                .fetch_optional(&mut **tx).await.map_err(db)?;
            if found.is_none() {
                return Err(missing());
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO personal_support_consents
              (authorization_id,user_id,owner_id,request_key,request_digest,ticket_id,purpose,resource_versions,content_field,created_at,expires_at)
             SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,t.seconds,t.seconds+$10 FROM
              (SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS seconds) t")
            .bind(&id).bind(user).bind(owner).bind(key).bind(digest).bind(terms.ticket_id).bind(terms.purpose)
            .bind(json!(terms.resources)).bind(terms.content_field).bind(i64::from(terms.duration_seconds))
            .execute(&mut **tx).await.map_err(db)?;
        id
    };
    let result = one(&mut guard, &id).await?;
    guard.finish().await?;
    Ok((StatusCode::CREATED, Json(result)).into_response())
}
pub(super) async fn get_one(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let result = one(&mut guard, &id).await?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn revoke(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    one(&mut guard, &id).await?;
    // The consent row is also the synchronization point for downstream reads.
    sqlx::query("SELECT authorization_id FROM personal_support_consents WHERE authorization_id=$1 FOR UPDATE")
        .bind(&id).fetch_one(&mut **guard.transaction()).await.map_err(db)?;
    sqlx::query("INSERT INTO personal_support_revocations(authorization_id,revoked_at)
                 VALUES($1,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint) ON CONFLICT DO NOTHING")
        .bind(&id).execute(&mut **guard.transaction()).await.map_err(db)?;
    let result = one(&mut guard, &id).await?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Query(q) = query.map_err(|_| AuthError::Invalid)?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) || q.before.as_deref().is_some_and(|id| !valid_id(id)) {
        return Err(AuthError::Invalid.into());
    }
    let mut guard = identity.authenticate(&headers).await?;
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let before = if let Some(before) = q.before {
        Some(sqlx::query_scalar::<_,i64>("SELECT sequence FROM personal_support_consents WHERE user_id=$1 AND owner_id=$2 AND authorization_id=$3")
            .bind(&user).bind(&owner).bind(before).fetch_optional(&mut **guard.transaction()).await.map_err(db)?.ok_or_else(missing)?)
    } else {
        None
    };
    let rows = sqlx::query(consent_query!(
        " AND ($3::bigint IS NULL OR c.sequence<$3) ORDER BY c.sequence DESC LIMIT $4"
    ))
    .bind(user)
    .bind(owner)
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut **guard.transaction())
    .await
    .map_err(db)?;
    let more = rows.len() > usize::from(limit);
    let entries = rows
        .into_iter()
        .take(usize::from(limit))
        .map(view)
        .collect::<Result<Vec<_>, _>>()?;
    let next = entries.last().map(|v| v["authorization_id"].clone());
    let mut page = json!({"data":entries});
    if more {
        page["next_before"] = next.ok_or(AuthError::Unavailable)?;
    }
    guard.finish().await?;
    Ok(Json(page))
}

/// Only metadata that the signed-in creator can offer in a new consent.
pub(super) async fn resources(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Query(q) = query.map_err(|_| AuthError::Invalid)?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) || q.before.as_deref().is_some_and(|s| !valid_id(s)) {
        return Err(AuthError::Invalid.into());
    }
    let mut guard = identity.authenticate(&headers).await?;
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let rows=sqlx::query("SELECT artifact_id,version_id,sha256,size_bytes FROM artifacts
        WHERE owner_id=$1 AND creator_id=$2 AND resource_id IS NULL AND state='available' AND media_type='text/plain'
          AND ($3::text IS NULL OR artifact_id<$3) ORDER BY artifact_id DESC LIMIT $4")
        .bind(&owner).bind(&user).bind(q.before).bind(i64::from(limit)+1)
        .fetch_all(&mut **guard.transaction()).await.map_err(db)?;
    let more = rows.len() > usize::from(limit);
    let mut entries = Vec::new();
    for row in rows.into_iter().take(usize::from(limit)) {
        entries.push(
            json!({"artifact_id":row.try_get::<String,_>("artifact_id").map_err(db)?,
            "version_id":row.try_get::<String,_>("version_id").map_err(db)?,
            "sha256":row.try_get::<String,_>("sha256").map_err(db)?,
            "size_bytes":row.try_get::<i64,_>("size_bytes").map_err(db)?.to_string()}),
        );
    }
    let next = entries.last().map(|v| v["artifact_id"].clone());
    let mut result = json!({"subject_user_id":user,"owner_id":owner,"data":entries});
    if more {
        result["next_before"] = next.ok_or(AuthError::Unavailable)?;
    }
    guard.finish().await?;
    Ok(Json(result))
}
