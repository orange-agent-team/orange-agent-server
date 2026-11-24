//! Authenticated reference metadata. Registration, price activation and worker review are separate operations.
use super::*;
use axum::extract::RawQuery;
use orange_model_runtime::manifest::{self, ManifestError};

async fn authenticate(
    identity: &UserIdentity,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<(), UserApiError> {
    let session = identity.authenticate(headers).await?;
    if query.is_some_and(|q| !q.is_empty()) {
        return Err(AuthError::Invalid.into());
    }
    session.finish().await?;
    Ok(())
}
pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, UserApiError> {
    authenticate(&identity, &headers, query.as_deref()).await?;
    let catalogue = manifest::bundled()
        .map_err(|_| UserApiError(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"))?;
    let items:Vec<_>=catalogue.iter().map(|item|{
        let r=item.receipt();
        serde_json::json!({"model_version":r.manifest.model_version,"manifest_sha256":r.manifest_sha256})
    }).collect();
    Ok(Json(serde_json::json!({"items":items})).into_response())
}
pub(super) async fn get_one(
    State(identity): State<Arc<UserIdentity>>,
    Path(version): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, UserApiError> {
    authenticate(&identity, &headers, query.as_deref()).await?;
    if !orange_protocol::valid_id(&version) {
        return Err(AuthError::Invalid.into());
    }
    let item = manifest::find(&version).map_err(|err| match err {
        ManifestError::NotFound => UserApiError(StatusCode::NOT_FOUND, "NOT_FOUND"),
        _ => UserApiError(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
    })?;
    Ok(Json(item.receipt()).into_response())
}
