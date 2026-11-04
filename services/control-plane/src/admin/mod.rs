pub mod identity;
pub mod routes;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

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
}
impl From<sqlx::Error> for AdminError {
    fn from(_: sqlx::Error) -> Self {
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
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        let retry = if matches!(self, Self::Unauthenticated | Self::StepUp) {
            "after_reauth"
        } else {
            "never"
        };
        let mut response=(status,Json(serde_json::json!({"code":self.to_string(),"message":self.to_string(),"request_id":uuid::Uuid::new_v4().to_string(),"retry_class":retry}))).into_response();
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
pub fn hash_secret(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub fn random_secret() -> String {
    openidconnect::CsrfToken::new_random_len(32)
        .secret()
        .clone()
}
pub fn timestamp(seconds: i64) -> Result<String, AdminError> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| AdminError::Invalid)?
        .format(&Rfc3339)
        .map_err(|_| AdminError::Invalid)
}
pub fn cookie(headers: &HeaderMap, name: &str) -> Result<String, AdminError> {
    let mut found = None;
    for header in headers.get_all("cookie") {
        let text = header.to_str().map_err(|_| AdminError::Unauthenticated)?;
        if text.len() > 8192 {
            return Err(AdminError::Unauthenticated);
        }
        for item in text.split(';') {
            if let Some((key, value)) = item.trim().split_once('=')
                && key == name
            {
                if found.is_some()
                    || value.len() < 32
                    || value.len() > 256
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                {
                    return Err(AdminError::Unauthenticated);
                }
                found = Some(value.to_owned());
            }
        }
    }
    found.ok_or(AdminError::Unauthenticated)
}
pub fn set_cookie(
    name: &str,
    value: &str,
    max_age: i64,
) -> Result<axum::http::HeaderValue, AdminError> {
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(AdminError::Invalid);
    }
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age}")
        .parse()
        .map_err(|_| AdminError::Invalid)
}
pub fn role_permissions() -> BTreeMap<String, BTreeSet<String>> {
    // Single source from the frozen E02 contract, not browser-supplied claims.
    let document: serde_json::Value =
        serde_json::from_str(include_str!("../../../../contracts/openapi/admin.v1.json"))
            .expect("checked embedded contract");
    serde_json::from_value(document["x-platform-role-permissions"].clone())
        .expect("checked permission registry")
}
