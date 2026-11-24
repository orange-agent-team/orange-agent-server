pub mod actions;
pub use actions::prices::activate_due as activate_due_text_prices;
pub mod identity;
pub mod model_review;
pub mod routes;

pub use crate::auth::{hash_secret, random_secret};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SESSION_COOKIE: &str = "__Host-orange-admin";
pub const FLOW_COOKIE: &str = "__Host-orange-admin-flow";

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("UNAUTHENTICATED")]
    Unauthenticated,
    #[error("FORBIDDEN")]
    Forbidden,
    #[error("CSRF_INVALID")]
    Csrf,
    #[error("STEP_UP_REQUIRED")]
    StepUp,
    #[error("VALIDATION_ERROR")]
    Invalid,
    #[error("SERVICE_UNAVAILABLE")]
    Unavailable,
    #[error("NOT_FOUND")]
    NotFound,
    #[error("ACTION_NOT_EXECUTABLE")]
    Conflict,
    #[error("VERSION_CONFLICT")]
    Precondition,
    #[error("IDEMPOTENCY_CONFLICT")]
    Idempotency,
}
impl From<crate::auth::AuthError> for AdminError {
    fn from(error: crate::auth::AuthError) -> Self {
        use crate::auth::AuthError;
        match error {
            AuthError::Unauthenticated => Self::Unauthenticated,
            AuthError::Forbidden => Self::Forbidden,
            AuthError::Csrf => Self::Csrf,
            AuthError::Context => Self::StepUp,
            AuthError::Invalid => Self::Invalid,
            AuthError::Unavailable => Self::Unavailable,
        }
    }
}
impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error().and_then(|v| v.code()).as_deref() == Some("OA086") {
            return Self::Conflict;
        }
        Self::Unavailable
    }
}
impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::Csrf | Self::StepUp => StatusCode::FORBIDDEN,
            Self::Invalid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict | Self::Idempotency => StatusCode::CONFLICT,
            Self::Precondition => StatusCode::PRECONDITION_FAILED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        let retry = if matches!(self, Self::Unauthenticated | Self::StepUp) {
            "after_reauth"
        } else {
            "never"
        };
        let code = self.to_string();
        let mut response = crate::diagnostics::typed_failure(
            status,
            orange_protocol::ApiError {
                message: code.clone(),
                code,
                request_id: uuid::Uuid::new_v4().to_string(),
                retry_class: retry.into(),
            },
        );
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub environment: String,
    pub kind: String,
    pub scope_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionView {
    pub principal_id: String,
    pub roles: Vec<String>,
    pub scopes: Vec<Scope>,
    pub permissions: Vec<String>,
    pub mfa_verified: bool,
    pub step_up_at: String,
    pub expires_at: String,
    pub csrf_token: String,
    pub policy_version: String,
}
#[derive(Clone)]
pub struct CurrentSession {
    pub view: SessionView,
    pub token_hash: String,
    pub auth_time: i64,
    pub now: i64,
    pub grants: BTreeMap<Scope, BTreeSet<String>>,
}
impl CurrentSession {
    pub fn authorize(&self, permission: &str, scope: &Scope) -> Result<(), AdminError> {
        if self
            .grants
            .get(scope)
            .is_some_and(|permissions| permissions.contains(permission))
        {
            Ok(())
        } else {
            Err(AdminError::Forbidden)
        }
    }
}
pub fn timestamp(seconds: i64) -> Result<String, AdminError> {
    crate::auth::timestamp(seconds).map_err(Into::into)
}
pub fn cookie(headers: &HeaderMap, name: &str) -> Result<String, AdminError> {
    crate::auth::cookie(headers, name).map_err(Into::into)
}
pub fn set_cookie(
    name: &str,
    value: &str,
    max_age: i64,
) -> Result<axum::http::HeaderValue, AdminError> {
    crate::auth::set_cookie(name, value, max_age).map_err(Into::into)
}
pub fn role_permissions() -> BTreeMap<String, BTreeSet<String>> {
    // Single source from the frozen E02 contract, not browser-supplied claims.
    let document: serde_json::Value =
        serde_json::from_str(include_str!("../../../../contracts/openapi/admin.v1.json"))
            .expect("checked embedded contract");
    serde_json::from_value(document["x-platform-role-permissions"].clone())
        .expect("checked permission registry")
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn emitted_errors_exist_in_the_frozen_contract() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../contracts/json-schema/admin.schema.json"
        ))
        .expect("embedded schema");
        let codes = schema["$defs"]["Error"]["properties"]["code"]["enum"]
            .as_array()
            .expect("codes");
        for error in [
            AdminError::Unauthenticated,
            AdminError::Forbidden,
            AdminError::Csrf,
            AdminError::StepUp,
            AdminError::Invalid,
            AdminError::Unavailable,
            AdminError::NotFound,
            AdminError::Conflict,
            AdminError::Precondition,
            AdminError::Idempotency,
        ] {
            assert!(
                codes.contains(&serde_json::json!(error.to_string())),
                "{}",
                error
            );
        }
    }
}
