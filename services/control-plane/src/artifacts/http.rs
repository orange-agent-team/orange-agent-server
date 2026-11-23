//! Optional authenticated user object endpoints. No development owner-ID header
//! or Worker credential can replace the current UserIdentity/CSRF boundary.
#[path = "job_socket.rs"]
mod socket;
use super::*;
use crate::auth::{AuthError, users::UserIdentity};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
#[derive(Clone)]
struct Api {
    identity: UserIdentity,
    store: ArtifactStore,
    downloads: Arc<Semaphore>,
    event_connections: socket::Admissions,
    #[cfg(unix)]
    audio: Option<AudioMeter>,
}
pub fn router(identity: UserIdentity, store: ArtifactStore) -> Router {
    build_router(
        identity,
        store,
        #[cfg(unix)]
        None,
    )
}
pub fn build_router(
    identity: UserIdentity,
    store: ArtifactStore,
    #[cfg(unix)] audio: Option<AudioMeter>,
) -> Router {
    let connectors = crate::auth::routes::hosted_connector_router(identity.clone(), store.clone());
    Router::new()
        .route(
            "/api/v1/owners/{owner}/quotes/{quote}/funding",
            get(quote_funding),
        )
        .route("/api/v1/agent/runs/{run}/model", get(cloud_status))
        .route(
            "/api/v1/agent/runs/{run}/model/quotes",
            axum::routing::post(cloud_quote),
        )
        .route(
            "/api/v1/agent/runs/{run}/model/submit",
            axum::routing::post(cloud_submit),
        )
        .route("/api/v1/agent/runs/{run}/model/result", get(cloud_result))
        .route(
            "/api/v1/agent/runs/{run}/model/continuation",
            axum::routing::post(cloud_continuation),
        )
        .route("/api/v1/agent/runs/{run}/model/history", get(cloud_history))
        .route(
            "/api/v1/owners/{owner}/uploads/{upload}/transcription-measurements",
            axum::routing::post(audio_measurement),
        )
        .route(
            "/api/v1/owners/{owner}/transcription-input-grants",
            axum::routing::post(create_audio_grant),
        )
        .route(
            "/api/v1/owners/{owner}/transcription-input-grants/{id}",
            get(audio_grant_status).delete(revoke_audio_grant),
        )
        .route("/api/v1/transcription-targets/{id}", get(audio_target))
        .route(
            "/api/v1/owners/{owner}/transcription-input-grants/{id}/prices",
            get(audio_prices),
        )
        .route(
            "/api/v1/owners/{owner}/transcription-quotes",
            axum::routing::post(audio_quote),
        )
        .route("/api/v1/models", get(market_models))
        .route("/api/v1/text-prices", get(prices))
        .route(
            "/api/v1/owners/{owner}/text-quotes",
            axum::routing::post(text_quote),
        )
        .route(
            "/api/v1/owners/{owner}/uploads",
            axum::routing::post(create),
        )
        .route(
            "/api/v1/owners/{owner}/uploads/{upload}",
            get(status).delete(delete),
        )
        .route(
            "/api/v1/owners/{owner}/uploads/{upload}/content",
            get(download).put(upload),
        )
        .route(
            "/api/v1/owners/{owner}/jobs",
            get(job_list).post(submit_job),
        )
        .route(
            "/api/v1/owners/{owner}/jobs/{job}/events/ws",
            get(socket::upgrade),
        )
        .route("/api/v1/owners/{owner}/jobs/{job}", get(job_status))
        .route("/api/v1/owners/{owner}/jobs/{job}/events", get(job_events))
        .route(
            "/api/v1/owners/{owner}/invocations/{invocation}/withdraw",
            axum::routing::post(withdraw_invocation),
        )
        .route(
            "/api/v1/owners/{owner}/jobs/by-invocation/{invocation}",
            get(job_lookup),
        )
        .route("/api/v1/owners/{owner}/jobs/{job}/result", get(job_result))
        .route(
            "/api/v1/owners/{owner}/jobs/{job}/cancel",
            axum::routing::post(cancel_job),
        )
        .route(
            "/api/v1/owners/{owner}/jobs/{job}/input",
            axum::routing::post(bind_input),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(Api {
            identity,
            store,
            downloads: Arc::new(Semaphore::new(2)),
            event_connections: socket::Admissions::new(),
            #[cfg(unix)]
            audio,
        })
        .merge(connectors)
}
async fn audio_prices(
    State(api): State<Api>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.audio_prices(&actor, &owner, &id).await?).into_response(),
    ))
}
async fn audio_quote(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    body: Result<
        Json<orange_protocol::pricing::CreateTranscriptionQuote>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(req) = body.map_err(|_| ArtifactError::Invalid)?;
    original_key(&headers, &req.request_id)?;
    Ok(private(
        (
            StatusCode::CREATED,
            Json(api.store.audio_quote(&actor, &owner, &req).await?),
        )
            .into_response(),
    ))
}
async fn audio_target(
    State(api): State<Api>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.audio_target(&actor, &id).await?).into_response(),
    ))
}
async fn create_audio_grant(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    body: Result<
        Json<orange_protocol::pricing::CreateTranscriptionInputGrant>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(r) = body.map_err(|_| ArtifactError::Invalid)?;
    original_key(&headers, &r.grant_id)?;
    #[cfg(unix)]
    {
        let meter = api.audio.as_ref().ok_or(ArtifactError::Unavailable)?;
        Ok(private(
            Json(
                api.store
                    .grant_audio_input(meter, &actor, &owner, &r)
                    .await?,
            )
            .into_response(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (actor, owner, r);
        Err(ArtifactError::Unavailable.into())
    }
}
async fn audio_grant_status(
    State(api): State<Api>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.audio_grant_status(&actor, &owner, &id).await?).into_response(),
    ))
}
async fn revoke_audio_grant(
    State(api): State<Api>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    original_key(&headers, &id)?;
    Ok(private(
        Json(api.store.revoke_audio_input(&actor, &owner, &id).await?).into_response(),
    ))
}
async fn audio_measurement(
    State(api): State<Api>,
    Path((owner, upload)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<
        Json<orange_protocol::pricing::MeasureAudio>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    original_key(&headers, &upload)?;
    let Json(request) = body.map_err(|_| ArtifactError::Invalid)?;
    #[cfg(unix)]
    {
        let meter = api.audio.as_ref().ok_or(ArtifactError::Unavailable)?;
        let measured = meter
            .measure(&api.store, &actor, &owner, &upload, &request.version_id)
            .await?;
        Ok(private(Json(measured).into_response()))
    }
    #[cfg(not(unix))]
    {
        let _ = (actor, owner, upload, request);
        Err(ArtifactError::Unavailable.into())
    }
}
struct Error(StatusCode, &'static str);
async fn quote_funding(
    State(api): State<Api>,
    Path((owner, quote)): Path<(String, String)>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, Error> {
    if query.as_deref().is_some_and(|q| !q.is_empty()) {
        return Err(ArtifactError::Invalid.into());
    }
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let view = api.store.funding(&actor, &owner, &quote).await?;
    Ok(private(Json(view).into_response()))
}

async fn market_models(
    State(api): State<Api>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    if query.as_deref().is_some_and(|query| !query.is_empty()) {
        return Err(ArtifactError::Invalid.into());
    }
    Ok(private(
        Json(api.store.market_models(&actor).await?).into_response(),
    ))
}

async fn prices(State(api): State<Api>, headers: HeaderMap) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.prices(&actor).await?).into_response(),
    ))
}
async fn text_quote(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    body: Result<
        Json<orange_protocol::pricing::CreateTextQuote>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response, Error> {
    let Json(request) = body.map_err(|_| ArtifactError::Invalid)?;
    original_key(&headers, &request.request_id)?;
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let quote = api
        .store
        .create_text_quote(&actor, &owner, &request)
        .await?;
    Ok(private((StatusCode::CREATED, Json(quote)).into_response()))
}
async fn job_result(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let result = api.store.job_result(&actor, &owner, &job).await?;
    Ok(private(Json(result).into_response()))
}
impl From<ArtifactError> for Error {
    fn from(e: ArtifactError) -> Self {
        match e {
            ArtifactError::Forbidden => Self(StatusCode::FORBIDDEN, "FORBIDDEN"),
            ArtifactError::NotFound => Self(StatusCode::NOT_FOUND, "NOT_FOUND"),
            ArtifactError::Deleted => Self(StatusCode::GONE, "ARTIFACT_DELETED"),
            ArtifactError::Invalid | ArtifactError::Input => {
                Self(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR")
            }
            ArtifactError::Integrity => {
                Self(StatusCode::UNPROCESSABLE_ENTITY, "INTEGRITY_MISMATCH")
            }
            ArtifactError::Conflict => Self(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"),
            ArtifactError::NotReady => Self(StatusCode::CONFLICT, "ARTIFACT_NOT_READY"),
            ArtifactError::InUse => Self(StatusCode::CONFLICT, "ARTIFACT_IN_USE"),
            ArtifactError::Busy => Self(StatusCode::CONFLICT, "TRANSFER_BUSY"),
            ArtifactError::Capacity => Self(StatusCode::INSUFFICIENT_STORAGE, "STORAGE_CAPACITY"),
            ArtifactError::Budget => Self(StatusCode::CONFLICT, "BUDGET_EXCEEDED"),
            ArtifactError::PricingUnavailable => {
                Self(StatusCode::SERVICE_UNAVAILABLE, "PRICING_UNAVAILABLE")
            }
            ArtifactError::Expired => Self(StatusCode::REQUEST_TIMEOUT, "TRANSFER_EXPIRED"),
            _ => Self(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
        }
    }
}
impl From<AuthError> for Error {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Unauthenticated => Self(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            AuthError::Csrf => Self(StatusCode::FORBIDDEN, "CSRF_INVALID"),
            AuthError::Forbidden | AuthError::Context => Self(StatusCode::FORBIDDEN, "FORBIDDEN"),
            AuthError::Invalid => Self(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
            AuthError::Unavailable => Self(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
        }
    }
}
fn private(mut r: Response) -> Response {
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("cross-origin-resource-policy", "same-origin"),
        ("referrer-policy", "no-referrer"),
    ] {
        r.headers_mut()
            .insert(k, axum::http::HeaderValue::from_static(v));
    }
    r
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        private(crate::diagnostics::typed_failure(
            self.0,
            orange_protocol::ApiError {
                code: self.1.into(),
                message: self.1.into(),
                request_id: id(),
                retry_class: if self.0 == StatusCode::UNAUTHORIZED {
                    "after_reauth"
                } else if self.0.is_server_error()
                    || matches!(
                        self.0,
                        StatusCode::REQUEST_TIMEOUT | StatusCode::PRECONDITION_FAILED
                    )
                {
                    "query_original"
                } else {
                    "never"
                }
                .into(),
            },
        ))
    }
}
fn original_key(headers: &HeaderMap, upload: &str) -> Result<(), Error> {
    if headers.get("idempotency-key").and_then(|v| v.to_str().ok()) != Some(upload) {
        return Err(ArtifactError::Conflict.into());
    }
    Ok(())
}
async fn create(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CreateUpload>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(req) = body.map_err(|_| ArtifactError::Invalid)?;
    original_key(&headers, &req.upload_id)?;
    Ok(private(
        Json(api.store.begin_upload(&actor, &owner, &req).await?).into_response(),
    ))
}
async fn status(
    State(api): State<Api>,
    Path((owner, upload)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.status(&actor, &owner, &upload).await?).into_response(),
    ))
}
async fn delete(
    State(api): State<Api>,
    Path((owner, upload)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    original_key(&headers, &upload)?;
    Ok(private(
        Json(
            api.store
                .delete_with_reference_check(&actor, &owner, &upload, true)
                .await?,
        )
        .into_response(),
    ))
}
async fn upload(
    State(api): State<Api>,
    Path((owner, upload)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    original_key(&headers, &upload)?;
    let input = body.into_data_stream().map(|r| {
        r.map(opendal::Buffer::from)
            .map_err(|_| ArtifactError::Input)
    });
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        api.store.put(actor, owner, upload, input),
    )
    .await
    .map_err(|_| ArtifactError::Expired)??;
    Ok(private(Json(result).into_response()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    version_id: String,
}
async fn download(
    State(api): State<Api>,
    Path((owner, upload)): Path<(String, String)>,
    headers: HeaderMap,
    query: Result<Query<Version>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let Query(query) = query.map_err(|_| ArtifactError::Invalid)?;
    let permit = api
        .downloads
        .clone()
        .try_acquire_owned()
        .map_err(|_| ArtifactError::Busy)?;
    let (view, data, stamp) = tokio::time::timeout(
        Duration::from_secs(60),
        api.store.read(&actor, &owner, &upload, &query.version_id),
    )
    .await
    .map_err(|_| ArtifactError::Expired)??;
    let length = view.size_bytes.clone();
    let etag = format!("\"{}\"", view.sha256);
    let (send, receive) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let store = api.store.clone();
    api.store
        .start_owned(async move {
            let _permit = permit;
            let bytes = data.to_bytes();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            for offset in (0..bytes.len()).step_by(64 * 1024) {
                let authority =
                    tokio::time::timeout_at(deadline, store.check_read(&actor, &view, &stamp))
                        .await;
                if !matches!(authority, Ok(Ok(()))) {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        send.send(Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "artifact authority changed",
                        ))),
                    )
                    .await;
                    return;
                }
                let end = (offset + 64 * 1024).min(bytes.len());
                if !matches!(
                    tokio::time::timeout_at(deadline, send.send(Ok(bytes.slice(offset..end))))
                        .await,
                    Ok(Ok(()))
                ) {
                    return;
                }
            }
        })
        .await?;
    let response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment")
        .header("content-length", length)
        .header("etag", etag)
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(receive),
        ))
        .map_err(|_| ArtifactError::Unavailable)?;
    Ok(private(response))
}

async fn submit_job(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    body: Result<Json<orange_protocol::SubmitJob>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(request) = body.map_err(|_| ArtifactError::Invalid)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(ArtifactError::Invalid)?;
    let job = api
        .store
        .submit_job(&actor, &owner, key, &request)
        .await
        .map_err(|e| match e {
            crate::store::StoreError::SpendingPaused => {
                Error(StatusCode::CONFLICT, "SPENDING_PAUSED")
            }
            crate::store::StoreError::InvocationWithdrawn => {
                Error(StatusCode::CONFLICT, "INVOCATION_WITHDRAWN")
            }
            crate::store::StoreError::Forbidden => Error(StatusCode::FORBIDDEN, "FORBIDDEN"),
            crate::store::StoreError::NotFound => Error(StatusCode::NOT_FOUND, "NOT_FOUND"),
            crate::store::StoreError::Database(_) | crate::store::StoreError::Migration(_) => {
                Error(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE")
            }
            _ => Error(StatusCode::CONFLICT, "JOB_SUBMISSION_REJECTED"),
        })?;
    let id = job.job_id.clone();
    Ok(crate::diagnostics::job_response(
        private(Json(job).into_response()),
        &id,
    ))
}
async fn bind_input(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<
        Json<orange_protocol::artifacts::BindJobInput>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    original_key(&headers, &job)?;
    let Json(request) = body.map_err(|_| ArtifactError::Invalid)?;
    Ok(private(
        Json(
            api.store
                .bind_job_input(&actor, &owner, &job, &request)
                .await?,
        )
        .into_response(),
    ))
}

async fn job_events(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
    query: Result<Query<super::job_events::HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let Query(query) = query.map_err(|_| ArtifactError::Invalid)?;
    Ok(private(
        Json(api.store.job_history(&actor, &owner, &job, &query).await?).into_response(),
    ))
}

async fn job_status(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    use orange_domain::identity::{JobId, OwnerId};
    let owner = OwnerId::try_from(owner.as_str()).map_err(|_| ArtifactError::Invalid)?;
    let job = JobId::try_from(job.as_str()).map_err(|_| ArtifactError::Invalid)?;
    Ok(private(
        api.store
            .submitted_job(&actor, owner, job)
            .await?
            .into_response(),
    ))
}

/// Strict empty body: user-requested cancellation has no arbitrary state,
/// refund amount, target identity or bypass flag supplied by the browser.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelJob {}
async fn cancel_job(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<CancelJob>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(_) = body.map_err(|_| ArtifactError::Invalid)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(ArtifactError::Invalid)?;
    let view = crate::store::Store::from_pool(api.store.inner.pool.clone())
        .cancel_as(
            Some(&actor),
            &owner,
            &job,
            key,
            crate::job_revision::Expected::from_headers(&headers)
                .map_err(|_| ArtifactError::Invalid)?,
        )
        .await
        .map_err(|e| match e {
            crate::store::StoreError::StaleJobVersion => {
                Error(StatusCode::PRECONDITION_FAILED, "STALE_JOB_VERSION")
            }
            crate::store::StoreError::Forbidden => Error(StatusCode::FORBIDDEN, "FORBIDDEN"),
            crate::store::StoreError::NotFound => Error(StatusCode::NOT_FOUND, "NOT_FOUND"),
            crate::store::StoreError::Invalid => {
                Error(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR")
            }
            crate::store::StoreError::Conflict => {
                Error(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT")
            }
            crate::store::StoreError::Database(_) | crate::store::StoreError::Migration(_) => {
                Error(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE")
            }
            _ => Error(StatusCode::CONFLICT, "CANCELLATION_REJECTED"),
        })?;
    Ok(private(Json(view).into_response()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobListQuery {
    after: Option<String>,
    limit: Option<u16>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoQuery {}
async fn job_lookup(
    State(api): State<Api>,
    Path((owner, invocation)): Path<(String, String)>,
    headers: HeaderMap,
    query: Result<Query<NoQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let Query(_) = query.map_err(|_| ArtifactError::Invalid)?;
    Ok(private(
        Json(
            api.store
                .job_by_invocation(&actor, &owner, &invocation)
                .await?,
        )
        .into_response(),
    ))
}
async fn job_list(
    State(api): State<Api>,
    Path(owner): Path<String>,
    headers: HeaderMap,
    query: Result<Query<JobListQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    let Query(query) = query.map_err(|_| ArtifactError::Invalid)?;
    let result = api
        .store
        .jobs_for_user(
            &actor,
            &owner,
            query.after.as_deref().unwrap_or(""),
            query.limit.unwrap_or(50),
        )
        .await?;
    Ok(private(Json(result).into_response()))
}

async fn withdraw_invocation(
    State(api): State<Api>,
    Path((owner, invocation)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<orange_protocol::SubmitJob>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(request) = body.map_err(|_| ArtifactError::Invalid)?;
    if request.invocation_id != invocation {
        return Err(ArtifactError::Invalid.into());
    }
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(ArtifactError::Invalid)?;
    let receipt = crate::store::Store::from_pool(api.store.inner.pool.clone())
        .withdraw_invocation(&actor, &owner, key, &request)
        .await
        .map_err(|e| match e {
            crate::store::StoreError::Forbidden => Error(StatusCode::FORBIDDEN, "FORBIDDEN"),
            crate::store::StoreError::NotFound => Error(StatusCode::NOT_FOUND, "NOT_FOUND"),
            crate::store::StoreError::Invalid => {
                Error(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR")
            }
            crate::store::StoreError::Database(_) | crate::store::StoreError::Migration(_) => {
                Error(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE")
            }
            _ => Error(StatusCode::CONFLICT, "WITHDRAWAL_REJECTED"),
        })?;
    Ok(private(Json(receipt).into_response()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyCloudRequest {}
fn cloud_key(h: &HeaderMap) -> Result<&str, Error> {
    h.get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_id(v))
        .ok_or_else(|| ArtifactError::Invalid.into())
}
async fn cloud_status(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.cloud_status(&actor, &run).await?).into_response(),
    ))
}
async fn cloud_quote(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
    body: Result<Json<EmptyCloudRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let _ = body.map_err(|_| ArtifactError::Invalid)?;
    let actor = api.identity.artifact_actor(&headers, true).await?;
    Ok(private(
        Json(
            api.store
                .cloud_quote(&actor, &run, cloud_key(&headers)?)
                .await?,
        )
        .into_response(),
    ))
}
async fn cloud_submit(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
    body: Result<Json<cloud_turn::Confirmation>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, true).await?;
    let Json(order) = body.map_err(|_| ArtifactError::Invalid)?;
    Ok(private(
        Json(
            api.store
                .confirm_cloud_model(&actor, &run, cloud_key(&headers)?, order)
                .await?,
        )
        .into_response(),
    ))
}
async fn cloud_result(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.cloud_result(&actor, &run).await?).into_response(),
    ))
}

async fn cloud_continuation(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
    body: Result<Json<EmptyCloudRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let _ = body.map_err(|_| ArtifactError::Invalid)?;
    let actor = api.identity.artifact_actor(&headers, true).await?;
    Ok(private(
        Json(
            api.store
                .prepare_continuation(&actor, &run, cloud_key(&headers)?)
                .await?,
        )
        .into_response(),
    ))
}

async fn cloud_history(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(run): Path<String>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    Ok(private(
        Json(api.store.cloud_history(&actor, &run).await?).into_response(),
    ))
}
