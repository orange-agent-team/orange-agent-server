//! Optional authenticated user object endpoints. No development owner-ID header
//! or Worker credential can replace the current UserIdentity/CSRF boundary.
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
}
pub fn router(identity: UserIdentity, store: ArtifactStore) -> Router {
    Router::new()
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
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(Api {
            identity,
            store,
            downloads: Arc::new(Semaphore::new(2)),
        })
}
struct Error(StatusCode, &'static str);
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
            ArtifactError::Busy => Self(StatusCode::CONFLICT, "TRANSFER_BUSY"),
            ArtifactError::Capacity => Self(StatusCode::INSUFFICIENT_STORAGE, "STORAGE_CAPACITY"),
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
        private(
            (
                self.0,
                Json(orange_protocol::ApiError {
                    code: self.1.into(),
                    message: self.1.into(),
                    request_id: id(),
                    retry_class: if self.0 == StatusCode::UNAUTHORIZED {
                        "after_reauth"
                    } else if self.0.is_server_error() || self.0 == StatusCode::REQUEST_TIMEOUT {
                        "query_original"
                    } else {
                        "never"
                    }
                    .into(),
                }),
            )
                .into_response(),
        )
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
        Json(api.store.delete(&actor, &owner, &upload).await?).into_response(),
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
