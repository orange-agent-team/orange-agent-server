//! Local development consumer API. This is deliberately not an OIDC/Admin implementation.
use crate::store::{Store, StoreError};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use orange_protocol::{ApiError, BudgetView, CreateBudget, JobView, SubmitJob, valid_id};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    identities: Arc<Vec<DevIdentity>>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevIdentity {
    pub owner_id: String,
    pub token: String,
}
impl ApiState {
    pub fn development(store: Store, identities: Vec<DevIdentity>) -> Result<Self, &'static str> {
        if identities.is_empty()
            || identities
                .iter()
                .any(|i| !valid_id(&i.owner_id) || i.token.len() < 32)
        {
            return Err(
                "development identities require valid owners and random tokens of at least 32 characters",
            );
        }
        for (index, item) in identities.iter().enumerate() {
            if identities[..index]
                .iter()
                .any(|i| i.token == item.token || i.owner_id == item.owner_id)
            {
                return Err("duplicate development identity");
            }
        }
        Ok(Self {
            store,
            identities: Arc::new(identities),
        })
    }
    fn owner(&self, headers: &HeaderMap) -> Result<String, HttpError> {
        let token = headers
            .get("authorization")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| HttpError::unauthenticated(headers))?;
        self.identities
            .iter()
            .find(|i| i.token == token)
            .map(|i| i.owner_id.clone())
            .ok_or_else(|| HttpError::unauthenticated(headers))
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/budgets", post(create_budget))
        .route("/api/v1/budgets/{id}", get(get_budget))
        .route("/api/v1/jobs", post(submit_job))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/cancel", post(cancel_job))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

async fn health(State(s): State<ApiState>) -> StatusCode {
    if sqlx::query("SELECT 1")
        .execute(s.store.pool())
        .await
        .is_ok()
    {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
async fn create_budget(
    State(s): State<ApiState>,
    h: HeaderMap,
    body: Result<Json<CreateBudget>, JsonRejection>,
) -> Result<Json<BudgetView>, HttpError> {
    let owner = s.owner(&h)?;
    let key = idempotency(&h)?;
    let Json(req) = body.map_err(|_| HttpError::invalid(&h))?;
    s.store
        .create_budget(&owner, &key, &req)
        .await
        .map(Json)
        .map_err(|e| HttpError::store(e, &h))
}
async fn get_budget(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<BudgetView>, HttpError> {
    let owner = s.owner(&h)?;
    s.store
        .budget(&owner, &id)
        .await
        .map(Json)
        .map_err(|e| HttpError::store(e, &h))
}
async fn submit_job(
    State(s): State<ApiState>,
    h: HeaderMap,
    body: Result<Json<SubmitJob>, JsonRejection>,
) -> Result<Json<JobView>, HttpError> {
    let owner = s.owner(&h)?;
    let key = idempotency(&h)?;
    let Json(req) = body.map_err(|_| HttpError::invalid(&h))?;
    s.store
        .submit(&owner, &key, &req)
        .await
        .map(Json)
        .map_err(|e| HttpError::store(e, &h))
}
async fn get_job(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, HttpError> {
    let owner = s.owner(&h)?;
    s.store
        .job(&owner, &id)
        .await
        .map(Json)
        .map_err(|e| HttpError::store(e, &h))
}
async fn cancel_job(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, HttpError> {
    let owner = s.owner(&h)?;
    let key = idempotency(&h)?;
    s.store
        .cancel_queued(&owner, &id, &key)
        .await
        .map(Json)
        .map_err(|e| HttpError::store(e, &h))
}
fn idempotency(h: &HeaderMap) -> Result<String, HttpError> {
    h.get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| valid_id(s))
        .map(str::to_owned)
        .ok_or_else(|| HttpError::invalid(h))
}
fn request_id(h: &HeaderMap) -> String {
    h.get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| valid_id(s))
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}
pub struct HttpError(StatusCode, ApiError);
impl HttpError {
    fn new(status: StatusCode, code: &str, retry: &str, h: &HeaderMap) -> Self {
        Self(
            status,
            ApiError {
                code: code.into(),
                message: code.into(),
                request_id: request_id(h),
                retry_class: retry.into(),
            },
        )
    }
    fn invalid(h: &HeaderMap) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", "never", h)
    }
    fn unauthenticated(h: &HeaderMap) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "after_reauth",
            h,
        )
    }
    fn store(e: StoreError, h: &HeaderMap) -> Self {
        let (status, code, retry) = match e {
            StoreError::Invalid => (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", "never"),
            StoreError::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN", "never"),
            StoreError::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND", "never"),
            StoreError::Conflict => (
                StatusCode::CONFLICT,
                "IDEMPOTENCY_CONFLICT",
                "query_original",
            ),
            StoreError::Budget => (StatusCode::CONFLICT, "BUDGET_EXCEEDED", "never"),
            StoreError::Balance => (StatusCode::CONFLICT, "INSUFFICIENT_BALANCE", "never"),
            StoreError::QuoteExpired => (StatusCode::CONFLICT, "QUOTE_EXPIRED", "never"),
            StoreError::QuoteMismatch => (StatusCode::CONFLICT, "QUOTE_INPUT_CHANGED", "never"),
            StoreError::InvalidTransition => {
                (StatusCode::CONFLICT, "INVALID_TRANSITION", "query_original")
            }
            StoreError::Database(_) | StoreError::Migration(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "STORAGE_UNAVAILABLE",
                "query_original",
            ),
        };
        Self::new(status, code, retry, h)
    }
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut r = (self.0, Json(self.1)).into_response();
        r.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
        r
    }
}
