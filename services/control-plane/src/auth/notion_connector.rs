//! Run-scoped Notion actions; authentication, grant and source health are checked on both sides of I/O.
use super::connector_access::{
    capacity, check_source, db, denied, disable_source, disabled, guard, key, missing,
};
use super::*;
use crate::{
    auth::users::UserSessionGuard,
    notion::{Action, Error, Scope, registry::Registered},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;

fn source(
    i: &UserIdentity,
    g: &UserSessionGuard,
    id: &str,
) -> Result<Arc<Registered>, UserApiError> {
    i.notion_sources
        .0
        .get(id)
        .filter(|s| s.owner == g.view().personal_owner_id)
        .cloned()
        .ok_or_else(missing)
}
fn map_source(e: Error) -> UserApiError {
    match e {
        Error::Denied => denied(),
        Error::Missing => missing(),
        Error::Invalid => AuthError::Invalid.into(),
        Error::Changed => UserApiError(StatusCode::CONFLICT, "DOCUMENT_SCOPE_CHANGED"),
        Error::Limit => UserApiError(StatusCode::PAYLOAD_TOO_LARGE, "DOCUMENT_LIMIT"),
        Error::RateLimited => UserApiError(StatusCode::TOO_MANY_REQUESTS, "SOURCE_RATE_LIMIT"),
        Error::Credentials => UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "SOURCE_REGISTRATION_DISABLED",
        ),
        Error::Protocol | Error::Unavailable => AuthError::Unavailable.into(),
    }
}
pub(super) async fn inventory(
    State(i): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let (mut g, active) = guard(&i, &h, &run, false, false).await?;
    let opt = super::external_grants::document_opted(&mut g, &run).await?;
    let faults = disabled(&mut g).await?;
    let sources: Vec<_> = i
        .notion_sources
        .0
        .iter()
        .filter(|(_, s)| s.owner == g.view().personal_owner_id && !faults.contains(&s.digest))
        .map(
            |(id, s)| json!({"source_id":id,"source_digest":s.digest,"roots":s.registration.roots}),
        )
        .collect();
    let grants:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('grant_id',c.grant_id,'source_id',c.source_id,'source_digest',c.source_digest,'root_keys',c.root_keys,'expires_at',c.expires_at,'revoked',r.grant_id IS NOT NULL) FROM external_source_grants c LEFT JOIN repository_source_revocations r USING(grant_id) WHERE c.run_id=$1 AND c.provider='notion' ORDER BY c.expires_at DESC,c.grant_id LIMIT 100").bind(run).fetch_all(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    Ok(Json(
        json!({"sources":sources,"recent_grants":grants,"run_readable":active,"document_access_selected":opt}),
    ))
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consent {
    source_id: String,
    source_digest: String,
    root_keys: Vec<String>,
    duration_seconds: u32,
}
pub(super) async fn grant(
    State(i): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    h: HeaderMap,
    body: Result<Json<Consent>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(mut c) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    if !super::external_grants::document_opted(&mut g, &run).await? {
        return Err(UserApiError(
            StatusCode::FORBIDDEN,
            "DOCUMENT_CONNECTORS_DISABLED",
        ));
    }
    let s = source(&i, &g, &c.source_id)?;
    check_source(&mut g, &s.digest).await?;
    c.root_keys.sort();
    if c.source_digest != s.digest
        || !(1..=3600).contains(&c.duration_seconds)
        || !(1..=100).contains(&c.root_keys.len())
        || c.root_keys.windows(2).any(|p| p[0] == p[1])
    {
        return Err(AuthError::Invalid.into());
    }
    if c.root_keys
        .iter()
        .any(|k| !s.registration.roots.contains_key(k))
    {
        return Err(denied());
    }
    let digest = orange_protocol::request_digest("notion.user-grant.v1", &(&run, &c))
        .map_err(|_| AuthError::Invalid)?;
    let grant = super::external_grants::Consent {
        run: &run,
        provider: "notion",
        source: &c.source_id,
        source_digest: &s.digest,
        roots: &c.root_keys,
        seconds: c.duration_seconds,
        key: key(&h)?,
        digest: &digest,
    }
    .issue(&mut g)
    .await?;
    g.finish().await?;
    Ok(Json(json!({"grant_id":grant})))
}
pub(super) async fn revoke(
    State(i): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<StatusCode, UserApiError> {
    let (mut g, _) = guard(&i, &h, &run, true, false).await?;
    super::external_grants::revoke(&mut g, &run, "notion", &id).await?;
    g.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn live(
    i: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    id: &str,
) -> Result<(Arc<Registered>, Scope), UserApiError> {
    let r=sqlx::query("SELECT source_id,source_digest,root_keys FROM external_source_grants c JOIN cloud_document_source_opt_ins USING(run_id) WHERE c.run_id=$1 AND c.provider='notion' AND c.grant_id=$2 AND c.expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM repository_source_revocations r WHERE r.grant_id=c.grant_id)").bind(run).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(denied)?;
    let s = source(i, g, r.get("source_id"))?;
    if s.digest != r.get::<String, _>("source_digest") {
        return Err(denied());
    }
    check_source(g, &s.digest).await?;
    Ok((
        s,
        Scope {
            grant: id.into(),
            allowed: r.get::<Vec<String>, _>("root_keys").into_iter().collect(),
        },
    ))
}
pub(super) async fn perform(
    State(i): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Action>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(action) = body.map_err(|_| AuthError::Invalid)?;
    let k = key(&h)?;
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    let (s, scope) = live(&i, &mut g, &run, &id).await?;
    super::connector_access::released_action(&mut g, "notion", &id, &action).await?;
    s.reader.authorize(&scope, &action).map_err(map_source)?;
    let digest = orange_protocol::request_digest("notion.action.v1", &(&run, &id, &action))
        .map_err(|_| AuthError::Invalid)?;
    if let Some(row)=sqlx::query("SELECT request_digest FROM repository_source_calls WHERE run_id=$1 AND provider='notion' AND request_key=$2").bind(&run).bind(k).fetch_optional(&mut **g.transaction()).await.map_err(db)?{
        return Err(UserApiError(StatusCode::CONFLICT,if row.get::<String,_>("request_digest")==digest{"REQUEST_ALREADY_CONSUMED"}else{"IDEMPOTENCY_CONFLICT"}));
    }
    capacity(&mut g, &run).await?;
    sqlx::query("INSERT INTO repository_source_calls(run_id,provider,grant_id,request_key,request_digest,until_at) VALUES($1,'notion',$2,$3,$4,clock_timestamp()+interval '35 seconds')").bind(&run).bind(&id).bind(k).bind(digest).execute(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    let result=s.reader.execute_guarded(&scope,action,||async{
        let (mut guard,_)=guard(&i,&h,&run,true,true).await.map_err(|_|Error::Denied)?;
        live(&i,&mut guard,&run,&id).await.map_err(|_|Error::Denied)?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM repository_source_calls WHERE run_id=$1 AND provider='notion' AND grant_id=$2 AND request_key=$3 AND until_at>clock_timestamp() AND finished_at IS NULL)")
            .bind(&run).bind(&id).bind(k).fetch_one(&mut **guard.transaction()).await.map_err(|_|Error::Unavailable)?;
        if !active{return Err(Error::Denied);}
        guard.finish().await.map_err(|_|Error::Denied)?;
        Ok(())
    }).await;
    if s.reader.authorization_refused() {
        disable_source(&i, &s.owner, "notion", &s.digest).await?;
    }
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    live(&i, &mut g, &run, &id).await?;
    super::external_grants::complete(&mut g, &run, "notion", &id, k).await?;
    g.finish().await?;
    result.map(Json).map_err(map_source)
}
