//! Confluence credentials belong to a fixed tenant. Attachments are quarantined outside public user uploads.
use super::connector_access::{
    capacity, check_source, db, denied, disable_source, disabled, guard, key, missing,
};
use super::external_grants::{self, document_opted};
use super::*;
use crate::{
    artifacts::{ArtifactError, ArtifactStore, QuarantineRequest},
    auth::users::UserSessionGuard,
    confluence::{Action, Attachment, Error, ResultData, Scope, registry::Registered},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
#[derive(Clone)]
struct Api {
    identity: Arc<UserIdentity>,
    objects: Option<ArtifactStore>,
}
pub(super) fn router(i: UserIdentity, objects: Option<ArtifactStore>) -> Router {
    Router::new()
        .route(
            "/api/v1/agent/runs/{run}/connectors/confluence",
            get(inventory).post(grant),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/confluence/{grant}",
            axum::routing::delete(revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/confluence/{grant}/actions",
            axum::routing::post(perform),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/confluence/{grant}/artifacts/{id}/content",
            axum::routing::post(download),
        )
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn(private_headers))
        .with_state(Api {
            identity: Arc::new(i),
            objects,
        })
}
fn source(
    i: &UserIdentity,
    g: &UserSessionGuard,
    id: &str,
) -> Result<Arc<Registered>, UserApiError> {
    i.confluence_sources
        .0
        .get(id)
        .filter(|s| s.owner == g.view().personal_owner_id)
        .cloned()
        .ok_or_else(missing)
}
fn source_error(e: Error) -> UserApiError {
    match e {
        Error::Denied => denied(),
        Error::Missing => missing(),
        Error::Invalid => AuthError::Invalid.into(),
        Error::Changed => UserApiError(StatusCode::CONFLICT, "CONFLUENCE_RESOURCE_CHANGED"),
        Error::Limit => UserApiError(StatusCode::PAYLOAD_TOO_LARGE, "CONFLUENCE_LIMIT"),
        Error::Credentials => UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "SOURCE_REGISTRATION_DISABLED",
        ),
        Error::RateLimited => UserApiError(StatusCode::TOO_MANY_REQUESTS, "SOURCE_RATE_LIMIT"),
        Error::Protocol | Error::Unavailable => AuthError::Unavailable.into(),
    }
}
fn storage_error(e: ArtifactError) -> UserApiError {
    match e {
        ArtifactError::Capacity => {
            UserApiError(StatusCode::INSUFFICIENT_STORAGE, "STORAGE_CAPACITY")
        }
        ArtifactError::NotReady | ArtifactError::NotFound => missing(),
        ArtifactError::Integrity => UserApiError(StatusCode::CONFLICT, "INTEGRITY_MISMATCH"),
        _ => AuthError::Unavailable.into(),
    }
}
async fn inventory(
    State(api): State<Api>,
    Path(run): Path<String>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let i = &api.identity;
    let (mut g, active) = guard(i, &h, &run, false, false).await?;
    let opt = document_opted(&mut g, &run).await?;
    let faults = disabled(&mut g).await?;
    let sources:Vec<_>=i.confluence_sources.0.iter().filter(|(_,s)|s.owner==g.view().personal_owner_id&&!faults.contains(&s.digest)).map(|(id,s)|
        json!({"source_id":id,"source_digest":s.digest,"site":s.registration.tenant,"spaces":s.registration.spaces})).collect();
    let grants:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('grant_id',c.grant_id,'source_id',c.source_id,'source_digest',c.source_digest,'space_keys',c.root_keys,'expires_at',c.expires_at,'revoked',r.grant_id IS NOT NULL) FROM external_source_grants c LEFT JOIN repository_source_revocations r USING(grant_id) WHERE c.run_id=$1 AND c.provider='confluence' ORDER BY c.expires_at DESC,c.grant_id LIMIT 100")
        .bind(run).fetch_all(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    Ok(Json(
        json!({"sources":sources,"recent_grants":grants,"run_readable":active,"document_access_selected":opt,"quarantine_available":api.objects.is_some()}),
    ))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Consent {
    source_id: String,
    source_digest: String,
    space_keys: Vec<String>,
    duration_seconds: u32,
}
async fn grant(
    State(api): State<Api>,
    Path(run): Path<String>,
    h: HeaderMap,
    body: Result<Json<Consent>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(mut c) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&api.identity, &h, &run, true, true).await?;
    if !document_opted(&mut g, &run).await? {
        return Err(UserApiError(
            StatusCode::FORBIDDEN,
            "DOCUMENT_CONNECTORS_DISABLED",
        ));
    }
    let s = source(&api.identity, &g, &c.source_id)?;
    check_source(&mut g, &s.digest).await?;
    c.space_keys.sort();
    if c.source_digest != s.digest
        || !(1..=3600).contains(&c.duration_seconds)
        || !(1..=100).contains(&c.space_keys.len())
        || c.space_keys.windows(2).any(|w| w[0] == w[1])
    {
        return Err(AuthError::Invalid.into());
    }
    if c.space_keys
        .iter()
        .any(|k| !s.registration.spaces.contains_key(k))
    {
        return Err(denied());
    }
    let digest = orange_protocol::request_digest("confluence.user-grant.v1", &(&run, &c))
        .map_err(|_| AuthError::Invalid)?;
    let id = external_grants::Consent {
        run: &run,
        provider: "confluence",
        source: &c.source_id,
        source_digest: &s.digest,
        roots: &c.space_keys,
        seconds: c.duration_seconds,
        key: key(&h)?,
        digest: &digest,
    }
    .issue(&mut g)
    .await?;
    g.finish().await?;
    Ok(Json(json!({"grant_id":id})))
}
async fn revoke(
    State(api): State<Api>,
    Path((run, id)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<StatusCode, UserApiError> {
    let (mut g, _) = guard(&api.identity, &h, &run, true, false).await?;
    external_grants::revoke(&mut g, &run, "confluence", &id).await?;
    g.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn live(
    i: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    id: &str,
) -> Result<(Arc<Registered>, Scope), UserApiError> {
    let row=sqlx::query("SELECT c.source_id,c.source_digest,c.root_keys FROM external_source_grants c JOIN cloud_document_source_opt_ins USING(run_id) WHERE c.run_id=$1 AND c.provider='confluence' AND c.grant_id=$2 AND c.expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM repository_source_revocations v WHERE v.grant_id=c.grant_id)").bind(run).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(denied)?;
    let s = source(i, g, row.get("source_id"))?;
    if s.digest != row.get::<String, _>("source_digest") {
        return Err(denied());
    }
    check_source(g, &s.digest).await?;
    Ok((
        s,
        Scope {
            grant: id.into(),
            spaces: row.get::<Vec<String>, _>("root_keys").into_iter().collect(),
        },
    ))
}
struct Call<'a> {
    i: &'a UserIdentity,
    h: &'a HeaderMap,
    run: &'a str,
    grant: &'a str,
    key: &'a str,
}
impl Call<'_> {
    async fn before_io(&self) -> Result<(), Error> {
        let (mut g, _) = guard(self.i, self.h, self.run, true, true)
            .await
            .map_err(|_| Error::Denied)?;
        live(self.i, &mut g, self.run, self.grant)
            .await
            .map_err(|_| Error::Denied)?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM repository_source_calls WHERE run_id=$1 AND provider='confluence' AND grant_id=$2 AND request_key=$3 AND finished_at IS NULL AND until_at>clock_timestamp())").bind(self.run).bind(self.grant).bind(self.key).fetch_one(&mut **g.transaction()).await.map_err(|_|Error::Unavailable)?;
        if !active {
            return Err(Error::Denied);
        }
        g.finish().await.map_err(|_| Error::Denied)?;
        Ok(())
    }
    async fn reserve(
        &self,
        g: &mut UserSessionGuard,
        digest: &str,
    ) -> Result<Option<Value>, UserApiError> {
        let row=sqlx::query("SELECT c.request_digest,c.grant_id,q.artifact_id,q.state,q.meta,q.sha256,q.size_bytes FROM repository_source_calls c LEFT JOIN confluence_quarantine q USING(run_id,provider,grant_id,request_key) WHERE c.run_id=$1 AND c.provider='confluence' AND c.request_key=$2")
            .bind(self.run).bind(self.key).fetch_optional(&mut **g.transaction()).await.map_err(db)?;
        if let Some(row) = row {
            if row.get::<String, _>("request_digest") != digest
                || row.get::<String, _>("grant_id") != self.grant
            {
                return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
            }
            if row.get::<Option<String>, _>("state").as_deref() == Some("available") {
                return Ok(Some(receipt(&row)?));
            }
            return Err(UserApiError(
                StatusCode::CONFLICT,
                "REQUEST_ALREADY_CONSUMED",
            ));
        }
        capacity(g, self.run).await?;
        sqlx::query("INSERT INTO repository_source_calls(run_id,provider,grant_id,request_key,request_digest,until_at) VALUES($1,'confluence',$2,$3,$4,clock_timestamp()+interval '35 seconds')").bind(self.run).bind(self.grant).bind(self.key).bind(digest).execute(&mut **g.transaction()).await.map_err(db)?;
        Ok(None)
    }
    async fn finish(&self, s: &Registered) -> Result<UserSessionGuard, UserApiError> {
        if s.reader.authorization_refused() {
            disable_source(self.i, &s.owner, "confluence", &s.digest).await?;
        }
        let (mut g, _) = guard(self.i, self.h, self.run, true, true).await?;
        live(self.i, &mut g, self.run, self.grant).await?;
        external_grants::complete(&mut g, self.run, "confluence", self.grant, self.key).await?;
        Ok(g)
    }
}
fn attachment_view(id: &str, m: &Attachment, sha: &str) -> Value {
    json!({"kind":"quarantined_artifact","artifact_id":id,"title":m.title,"size_bytes":m.bytes.to_string(),"version":m.version.to_string(),"sha256":sha})
}
fn receipt(row: &sqlx::postgres::PgRow) -> Result<Value, UserApiError> {
    let m: Attachment =
        serde_json::from_value(row.get("meta")).map_err(|_| AuthError::Unavailable)?;
    Ok(attachment_view(
        row.get("artifact_id"),
        &m,
        row.get("sha256"),
    ))
}
async fn perform(
    State(api): State<Api>,
    Path((run, id)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Action>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(action) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&api.identity, &h, &run, true, true).await?;
    let (s, scope) = live(&api.identity, &mut g, &run, &id).await?;
    super::connector_access::released_action(&mut g, "confluence", &id, &action).await?;
    s.reader.authorize(&scope, &action).map_err(source_error)?;
    let call = Call {
        i: &api.identity,
        h: &h,
        run: &run,
        grant: &id,
        key: key(&h)?,
    };
    let digest = orange_protocol::request_digest("confluence.action.v1", &(&run, &id, &action))
        .map_err(|_| AuthError::Invalid)?;
    if let Some(old) = call.reserve(&mut g, &digest).await? {
        g.finish().await?;
        return Ok(Json(json!({"kind":"external_untrusted_data","data":old})));
    }
    let reservation = if let (Action::ReadAttachment { space, .. }, Some(m)) = (
        &action,
        s.reader
            .expected_attachment(&scope, &action)
            .map_err(source_error)?,
    ) {
        let store = api.objects.as_ref().ok_or(UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "QUARANTINE_UNAVAILABLE",
        ))?;
        let owner = g.view().personal_owner_id.clone();
        let artifact = store
            .reserve_quarantine(
                g.transaction(),
                QuarantineRequest {
                    run: &run,
                    owner: &owner,
                    grant: &id,
                    request: call.key,
                    space,
                    attachment: &m,
                },
            )
            .await
            .map_err(storage_error)?;
        Some((store, artifact))
    } else {
        None
    };
    g.finish().await?;
    let result = s.reader.execute(&scope, action, || call.before_io()).await;
    match result {
        Ok(ResultData::Attachment {
            meta,
            bytes,
            sha256,
        }) => {
            let (store, artifact) = reservation.ok_or(AuthError::Unavailable)?;
            call.before_io().await.map_err(source_error)?;
            store
                .stage_quarantine(&artifact, &meta, bytes, &sha256)
                .await
                .map_err(storage_error)?;
            let mut g = call.finish(&s).await?;
            store
                .publish_quarantine(g.transaction(), &artifact)
                .await
                .map_err(storage_error)?;
            g.finish().await?;
            Ok(Json(
                json!({"kind":"external_untrusted_data","data":attachment_view(&artifact,&meta,&sha256)}),
            ))
        }
        other => {
            let g = call.finish(&s).await?;
            g.finish().await?;
            match other.map_err(source_error)? {
                ResultData::Document(v) => Ok(Json(v)),
                _ => Err(AuthError::Unavailable.into()),
            }
        }
    }
}
async fn download(
    State(api): State<Api>,
    Path((run, id, artifact)): Path<(String, String, String)>,
    h: HeaderMap,
) -> Result<Response, UserApiError> {
    let store = api.objects.as_ref().ok_or(UserApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "QUARANTINE_UNAVAILABLE",
    ))?;
    let (mut g, _) = guard(&api.identity, &h, &run, true, true).await?;
    let (s, scope) = live(&api.identity, &mut g, &run, &id).await?;
    super::connector_access::released(&mut g, "confluence", &id, "read_attachment").await?;
    let row=sqlx::query("SELECT q.meta,q.space_key FROM confluence_quarantine q JOIN confluence_quarantine_authority a USING(artifact_id) WHERE q.run_id=$1 AND q.grant_id=$2 AND q.artifact_id=$3 AND q.state='available'").bind(&run).bind(&id).bind(&artifact).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(missing)?;
    let meta: Attachment =
        serde_json::from_value(row.get("meta")).map_err(|_| AuthError::Unavailable)?;
    let call = Call {
        i: &api.identity,
        h: &h,
        run: &run,
        grant: &id,
        key: key(&h)?,
    };
    let digest = orange_protocol::request_digest("confluence.download.v1", &(&run, &id, &artifact))
        .map_err(|_| AuthError::Invalid)?;
    call.reserve(&mut g, &digest).await?;
    g.finish().await?;
    let actor = api.identity.artifact_actor(&h, true).await?;
    let bytes = store
        .read_quarantine(&actor, &artifact, &run, &id)
        .await
        .map_err(storage_error)?;
    let checked = s
        .reader
        .current_attachment(&scope, row.get("space_key"), &meta, || call.before_io())
        .await;
    if matches!(checked, Err(Error::Changed | Error::Missing)) {
        store
            .discard_quarantine(&artifact, &run, &id)
            .await
            .map_err(storage_error)?;
    }
    let g = call.finish(&s).await?;
    g.finish().await?;
    checked.map_err(source_error)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
