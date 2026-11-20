//! Personal CN-05 consent. The user session and live grant are checked again after source IO.
use super::connector_access::{check_source, disable_source, disabled};
use super::*;
use crate::{
    auth::users::UserSessionGuard,
    pg_view_sources::Source,
    pg_views::{Action, Error},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;

use super::connector_access::{capacity, db, denied, guard, key, missing};
fn source(identity: &UserIdentity, owner: &str, id: &str) -> Result<Arc<Source>, UserApiError> {
    identity
        .pg_sources
        .0
        .get(id)
        .filter(|s| s.owner_id == owner)
        .cloned()
        .ok_or_else(missing)
}
pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let (mut g, run_readable) = guard(&identity, &headers, &run, false, false).await?;
    let faults = disabled(&mut g).await?;
    let sources:Vec<_>=identity.pg_sources.0.iter().filter(|(_,s)|s.owner_id==g.view().personal_owner_id && !faults.contains(&s.digest)).map(|(id,s)|json!({"source_id":id,"source_digest":s.digest,"view_ids":s.registration.views.keys().collect::<Vec<_>>()})).collect();
    let grants:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('grant_id',g.grant_id,'source_id',g.source_id,'source_digest',g.source_digest,'view_ids',g.view_ids,'expires_at',g.expires_at,'revoked',v.grant_id IS NOT NULL) FROM pg_view_grants g LEFT JOIN pg_view_revocations v USING(grant_id) WHERE g.run_id=$1 ORDER BY expires_at DESC,grant_id LIMIT 100").bind(run).fetch_all(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    Ok(Json(
        json!({"sources":sources,"recent_grants":grants,"run_readable":run_readable}),
    ))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consent {
    source_id: String,
    source_digest: String,
    view_ids: Vec<String>,
    duration_seconds: u32,
}
pub(super) async fn grant(
    State(identity): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Consent>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(mut c) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&identity, &headers, &run, true, true).await?;
    let s = source(&identity, &g.view().personal_owner_id, &c.source_id)?;
    check_source(&mut g, &s.digest).await?;
    c.view_ids.sort();
    if s.digest != c.source_digest
        || !(1..=3600).contains(&c.duration_seconds)
        || !(1..=100).contains(&c.view_ids.len())
        || c.view_ids.windows(2).any(|w| w[0] == w[1])
        || c.view_ids
            .iter()
            .any(|id| !s.registration.views.contains_key(id))
    {
        return Err(AuthError::Invalid.into());
    }
    let digest = orange_protocol::request_digest("cn05.user-consent.v1", &(&run, &c))
        .map_err(|_| AuthError::Invalid)?;
    let tx = g.transaction();
    let key = key(&headers)?;
    let grant = if let Some(old) = sqlx::query(
        "SELECT grant_id,request_digest FROM pg_view_grants WHERE run_id=$1 AND request_key=$2",
    )
    .bind(&run)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    {
        if old.get::<String, _>("request_digest") != digest {
            return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
        }
        old.get("grant_id")
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO pg_view_grants(grant_id,run_id,source_id,source_digest,view_ids,request_key,request_digest,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+make_interval(secs=>$8))").bind(&id).bind(run).bind(c.source_id).bind(s.digest.as_str()).bind(c.view_ids).bind(key).bind(digest).bind(f64::from(c.duration_seconds)).execute(&mut **tx).await.map_err(db)?;
        id
    };
    g.finish().await?;
    Ok(Json(json!({"grant_id":grant})))
}
pub(super) async fn revoke(
    State(identity): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, UserApiError> {
    let (mut g, _) = guard(&identity, &headers, &run, true, false).await?;
    let found: Option<String> =
        sqlx::query_scalar("SELECT grant_id FROM pg_view_grants WHERE run_id=$1 AND grant_id=$2")
            .bind(run)
            .bind(&id)
            .fetch_optional(&mut **g.transaction())
            .await
            .map_err(db)?;
    found.ok_or_else(missing)?;
    sqlx::query("INSERT INTO pg_view_revocations(grant_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut **g.transaction())
        .await
        .map_err(db)?;
    g.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn authority(
    identity: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    id: &str,
) -> Result<(Arc<Source>, BTreeSet<String>), UserApiError> {
    let row=sqlx::query("SELECT source_id,source_digest,view_ids FROM pg_view_grants c WHERE run_id=$1 AND grant_id=$2 AND expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM pg_view_revocations WHERE grant_id=c.grant_id)").bind(run).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(denied)?;
    let s = source(identity, &g.view().personal_owner_id, row.get("source_id"))?;
    check_source(g, &s.digest).await?;
    if s.digest != row.get::<String, _>("source_digest") {
        return Err(denied());
    }
    Ok((
        s,
        row.get::<Vec<String>, _>("view_ids").into_iter().collect(),
    ))
}
pub(super) async fn perform(
    State(identity): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Action>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(action) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&identity, &headers, &run, true, true).await?;
    let (s, views) = authority(&identity, &mut g, &run, &id).await?;
    let request = key(&headers)?;
    capacity(&mut g, &run).await?;
    let tx = g.transaction();
    let inserted=sqlx::query("INSERT INTO pg_view_calls(run_id,request_key,grant_id,until_at) VALUES($1,$2,$3,clock_timestamp()+interval '35 seconds') ON CONFLICT DO NOTHING").bind(&run).bind(request).bind(&id).execute(&mut **tx).await.map_err(db)?;
    if inserted.rows_affected() != 1 {
        return Err(UserApiError(
            StatusCode::CONFLICT,
            "REQUEST_ALREADY_CONSUMED",
        ));
    }
    g.finish().await?;
    let result = s.registration.execute(&s.pool, &views, action).await;
    if matches!(result, Err(Error::SourceAuthority)) {
        disable_source(&identity, &s.owner_id, "postgres", &s.digest).await?;
    }
    let (mut g, _) = guard(&identity, &headers, &run, true, true).await?;
    authority(&identity, &mut g, &run, &id).await?;
    let current=sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1 AND request_key=$2 AND finished_at IS NULL AND until_at>clock_timestamp()").bind(&run).bind(request).execute(&mut **g.transaction()).await.map_err(db)?;
    if current.rows_affected() != 1 {
        return Err(denied());
    }
    g.finish().await?;
    result.map(Json).map_err(|e| match e {
        Error::SourceAuthority => UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "SOURCE_REGISTRATION_DISABLED",
        ),
        Error::Denied => denied(),
        Error::Invalid => AuthError::Invalid.into(),
        Error::Changed => UserApiError(StatusCode::CONFLICT, "SOURCE_CHANGED"),
        Error::Limit => UserApiError(StatusCode::PAYLOAD_TOO_LARGE, "CONNECTOR_RESULT_TOO_LARGE"),
        Error::Unavailable => AuthError::Unavailable.into(),
    })
}
