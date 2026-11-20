//! Run-scoped external S3 reads. A receipt refers to one immutable byte identity.
use super::connector_access::{capacity, db, denied, guard, key, missing};
use super::connector_access::{check_source, disable_source, disabled};
use super::*;
use crate::{
    auth::users::UserSessionGuard,
    s3_objects::{Error, Head},
    s3_sources::Registered,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;

fn registered(i: &UserIdentity, owner: &str, id: &str) -> Result<Arc<Registered>, UserApiError> {
    i.s3_sources
        .0
        .get(id)
        .filter(|s| s.owner == owner)
        .cloned()
        .ok_or_else(missing)
}
pub(super) async fn live(
    i: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    id: &str,
) -> Result<Arc<Registered>, UserApiError> {
    let row=sqlx::query("SELECT source_id,source_digest FROM s3_source_grants g WHERE run_id=$1 AND grant_id=$2 AND expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM s3_source_revocations r WHERE r.grant_id=g.grant_id)")
        .bind(run).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(denied)?;
    let source = registered(i, &g.view().personal_owner_id, row.get("source_id"))?;
    check_source(g, &source.digest).await?;
    if source.digest != row.get::<String, _>("source_digest") {
        return Err(denied());
    }
    Ok(source)
}
fn source_error(error: Error) -> UserApiError {
    match error {
        Error::Invalid => AuthError::Invalid.into(),
        Error::Denied => denied(),
        Error::Missing => missing(),
        Error::Changed => UserApiError(StatusCode::CONFLICT, "OBJECT_CHANGED"),
        Error::Limit => UserApiError(StatusCode::PAYLOAD_TOO_LARGE, "OBJECT_TOO_LARGE"),
        Error::Busy => UserApiError(StatusCode::TOO_MANY_REQUESTS, "SOURCE_RATE_LIMIT"),
        Error::Unavailable => AuthError::Unavailable.into(),
    }
}
pub(super) async fn list(
    State(i): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let (mut g, readable) = guard(&i, &headers, &run, false, false).await?;
    let faults = disabled(&mut g).await?;
    let sources:Vec<_>=i.s3_sources.0.iter().filter(|(_,s)|s.owner==g.view().personal_owner_id && !faults.contains(&s.digest)).map(|(id,s)|json!({"source_id":id,"source_digest":s.digest,"bucket":s.registration.bucket,"prefix":s.registration.prefix})).collect();
    let grants:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('grant_id',g.grant_id,'source_id',g.source_id,'source_digest',g.source_digest,'expires_at',g.expires_at,'revoked',r.grant_id IS NOT NULL) FROM s3_source_grants g LEFT JOIN s3_source_revocations r USING(grant_id) WHERE run_id=$1 ORDER BY expires_at DESC,grant_id LIMIT 100")
        .bind(run).fetch_all(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    Ok(Json(
        json!({"sources":sources,"recent_grants":grants,"run_readable":readable}),
    ))
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consent {
    source_id: String,
    source_digest: String,
    duration_seconds: u32,
}
pub(super) async fn grant(
    State(i): State<Arc<UserIdentity>>,
    Path(run): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Consent>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(c) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&i, &headers, &run, true, true).await?;
    let s = registered(&i, &g.view().personal_owner_id, &c.source_id)?;
    check_source(&mut g, &s.digest).await?;
    if s.digest != c.source_digest || !(1..=3600).contains(&c.duration_seconds) {
        return Err(AuthError::Invalid.into());
    }
    let req = key(&headers)?;
    let digest = orange_protocol::request_digest("cn06.consent.v1", &(&run, &c))
        .map_err(|_| AuthError::Invalid)?;
    let tx = g.transaction();
    let id = if let Some(row) = sqlx::query(
        "SELECT grant_id,request_digest FROM s3_source_grants WHERE run_id=$1 AND request_key=$2",
    )
    .bind(&run)
    .bind(req)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    {
        if row.get::<String, _>("request_digest") != digest {
            return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
        }
        row.get("grant_id")
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO s3_source_grants(grant_id,run_id,source_id,source_digest,request_key,request_digest,expires_at) VALUES($1,$2,$3,$4,$5,$6,clock_timestamp()+make_interval(secs=>$7))").bind(&id).bind(run).bind(c.source_id).bind(c.source_digest).bind(req).bind(digest).bind(f64::from(c.duration_seconds)).execute(&mut **tx).await.map_err(db)?;
        id
    };
    g.finish().await?;
    Ok(Json(json!({"grant_id":id})))
}
pub(super) async fn revoke(
    State(i): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, UserApiError> {
    let (mut g, _) = guard(&i, &headers, &run, true, false).await?;
    sqlx::query("SELECT grant_id FROM s3_source_grants WHERE run_id=$1 AND grant_id=$2")
        .bind(run)
        .bind(&id)
        .fetch_optional(&mut **g.transaction())
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
    sqlx::query("INSERT INTO s3_source_revocations(grant_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut **g.transaction())
        .await
        .map_err(db)?;
    g.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Action {
    ListObjects { after: Option<String>, limit: u16 },
    HeadObject { key: String },
    ReadObject { expected: Head },
}
enum ResultData {
    Metadata(Value),
    Artifact { head: Head, digest: String },
}
fn artifact(id: &str, head: Value, digest: &str) -> Value {
    json!({"kind":"source_backed_artifact","artifact_id":id,"head":head,"sha256":digest})
}
async fn admit(
    g: &mut UserSessionGuard,
    run: &str,
    id: &str,
    req: &str,
    digest: &str,
) -> Result<Option<Value>, UserApiError> {
    let row=sqlx::query("SELECT c.grant_id,c.request_digest,a.artifact_id,a.head,a.content_sha256 FROM s3_source_calls c LEFT JOIN s3_artifact_receipts a USING(run_id,request_key,grant_id) WHERE c.run_id=$1 AND c.request_key=$2")
        .bind(run).bind(req).fetch_optional(&mut **g.transaction()).await.map_err(db)?;
    if let Some(old) = row {
        if old.get::<String, _>("grant_id") != id
            || old.get::<String, _>("request_digest") != digest
        {
            return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
        }
        if let Some(existing) = old.get::<Option<String>, _>("artifact_id") {
            return Ok(Some(artifact(
                &existing,
                old.get("head"),
                old.get("content_sha256"),
            )));
        }
        return Err(UserApiError(
            StatusCode::CONFLICT,
            "REQUEST_ALREADY_CONSUMED",
        ));
    }
    capacity(g, run).await?;
    sqlx::query("INSERT INTO s3_source_calls(run_id,request_key,grant_id,request_digest,until_at) VALUES($1,$2,$3,$4,clock_timestamp()+interval '35 seconds')").bind(run).bind(req).bind(id).bind(digest).execute(&mut **g.transaction()).await.map_err(db)?;
    Ok(None)
}
async fn delivered(
    i: &UserIdentity,
    h: &HeaderMap,
    run: &str,
    id: &str,
    req: &str,
) -> Result<UserSessionGuard, UserApiError> {
    let (mut g, _) = guard(i, h, run, true, true).await?;
    live(i, &mut g, run, id).await?;
    let changed=sqlx::query("UPDATE s3_source_calls SET finished_at=clock_timestamp() WHERE run_id=$1 AND request_key=$2 AND grant_id=$3 AND finished_at IS NULL AND until_at>clock_timestamp()").bind(run).bind(req).bind(id).execute(&mut **g.transaction()).await.map_err(db)?;
    if changed.rows_affected() != 1 {
        return Err(denied());
    }
    Ok(g)
}
pub(super) async fn perform(
    State(i): State<Arc<UserIdentity>>,
    Path((run, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Action>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(action) = body.map_err(|_| AuthError::Invalid)?;
    perform_checked(i, run, id, headers, action, None).await
}
pub(super) async fn perform_checked(
    i: Arc<UserIdentity>,
    run: String,
    id: String,
    headers: HeaderMap,
    action: Action,
    permit: Option<&super::model_connectors::ReadPermit<'_>>,
) -> Result<Json<Value>, UserApiError> {
    let protected = permit.map(|p| p.key());
    let req = protected.as_deref().unwrap_or(key(&headers)?);
    let digest = orange_protocol::request_digest("cn06.read.v1", &(&run, &id, &action))
        .map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&i, &headers, &run, true, true).await?;
    if let Some(p) = permit {
        p.check(&mut g, &run, &id).await?;
    }
    let source = live(&i, &mut g, &run, &id).await?;
    super::connector_access::released_action(&mut g, "s3", &id, &action).await?;
    if let Some(prior) = admit(&mut g, &run, &id, req, &digest).await? {
        g.finish().await?;
        return Ok(Json(json!({"kind":"external_untrusted_data","data":prior})));
    }
    g.finish().await?;
    let fetched = match action {
        Action::ListObjects { after, limit } => source
            .reader
            .list(after.as_deref(), usize::from(limit))
            .await
            .map(|ls| ResultData::Metadata(json!(ls))),
        Action::HeadObject { key } => source
            .reader
            .head(&key)
            .await
            .map(|h| ResultData::Metadata(json!(h))),
        Action::ReadObject { expected } => {
            source
                .reader
                .read(&expected)
                .await
                .map(|r| ResultData::Artifact {
                    head: r.head,
                    digest: r.sha256,
                })
        }
    };
    if source.reader.authorization_refused() {
        disable_source(&i, &source.owner, "s3", &source.digest).await?;
    }
    let mut g = delivered(&i, &headers, &run, &id, req).await?;
    if let Some(p) = permit {
        p.check(&mut g, &run, &id).await?;
    }
    let data = match fetched {
        Ok(ResultData::Metadata(value)) => value,
        Ok(ResultData::Artifact { head, digest }) => {
            let artifact_id = uuid::Uuid::new_v4().to_string();
            let head = json!(head);
            sqlx::query("INSERT INTO s3_artifact_receipts(artifact_id,run_id,request_key,grant_id,head,content_sha256) VALUES($1,$2,$3,$4,$5,$6)").bind(&artifact_id).bind(&run).bind(req).bind(&id).bind(&head).bind(&digest).execute(&mut **g.transaction()).await.map_err(db)?;
            artifact(&artifact_id, head, &digest)
        }
        Err(error) => {
            g.finish().await?;
            return Err(source_error(error));
        }
    };
    g.finish().await?;
    Ok(Json(json!({"kind":"external_untrusted_data","data":data})))
}
pub(super) async fn download(
    State(i): State<Arc<UserIdentity>>,
    Path((run, id, artifact_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, UserApiError> {
    let (mut g, _) = guard(&i, &headers, &run, true, true).await?;

    let source = live(&i, &mut g, &run, &id).await?;
    super::connector_access::released(&mut g, "s3", &id, "read_object").await?;
    let saved=sqlx::query("SELECT head,content_sha256 FROM s3_artifact_receipts WHERE artifact_id=$1 AND run_id=$2 AND grant_id=$3").bind(&artifact_id).bind(&run).bind(&id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(missing)?;
    let head: Head =
        serde_json::from_value(saved.get("head")).map_err(|_| AuthError::Unavailable)?;
    let req = key(&headers)?;
    let digest =
        orange_protocol::request_digest("cn06.artifact-content.v1", &(&run, &id, &artifact_id))
            .map_err(|_| AuthError::Invalid)?;
    admit(&mut g, &run, &id, req, &digest).await?;
    g.finish().await?;
    let fetched = source.reader.read(&head).await;
    if source.reader.authorization_refused() {
        disable_source(&i, &source.owner, "s3", &source.digest).await?;
    }
    let g = delivered(&i, &headers, &run, &id, req).await?;
    g.finish().await?;
    let read = fetched.map_err(source_error)?;
    if read.sha256 != saved.get::<String, _>("content_sha256") {
        return Err(source_error(Error::Changed));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        read.bytes,
    )
        .into_response())
}
