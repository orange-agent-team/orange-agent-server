//! Shared protocol primitives; user and administrator sessions/permissions remain separate.
pub mod oidc;
pub mod routes;
pub mod users;
use axum::http::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("UNAUTHENTICATED")]
    Unauthenticated,
    #[error("FORBIDDEN")]
    Forbidden,
    #[error("CSRF_INVALID")]
    Csrf,
    #[error("AUTH_CONTEXT_REQUIRED")]
    Context,
    #[error("VALIDATION_ERROR")]
    Invalid,
    #[error("SERVICE_UNAVAILABLE")]
    Unavailable,
}
impl From<sqlx::Error> for AuthError {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
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
pub fn timestamp(seconds: i64) -> Result<String, AuthError> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| AuthError::Invalid)?
        .format(&Rfc3339)
        .map_err(|_| AuthError::Invalid)
}
pub fn cookie(headers: &HeaderMap, name: &str) -> Result<String, AuthError> {
    let mut found = None;
    for header in headers.get_all("cookie") {
        let text = header.to_str().map_err(|_| AuthError::Unauthenticated)?;
        if text.len() > 8192 {
            return Err(AuthError::Unauthenticated);
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
                    return Err(AuthError::Unauthenticated);
                }
                found = Some(value.to_owned());
            }
        }
    }
    found.ok_or(AuthError::Unauthenticated)
}
pub fn set_cookie(name: &str, value: &str, max_age: i64) -> Result<HeaderValue, AuthError> {
    if !name.starts_with("__Host-")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        || !(0..=8 * 3600).contains(&max_age)
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(AuthError::Invalid);
    }
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age}")
        .parse()
        .map_err(|_| AuthError::Invalid)
}

fn single_write_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let only = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(only)
}

/// A mutation must have one unambiguous browser origin, CSRF proof and logical request identity.
pub fn check_write(headers: &HeaderMap, origin: &str, csrf: &str) -> Result<(), AuthError> {
    if single_write_header(headers, "origin") != Some(origin.trim_end_matches('/')) {
        return Err(AuthError::Csrf);
    }
    let supplied = single_write_header(headers, "x-csrf-token").ok_or(AuthError::Csrf)?;
    if supplied.len() > 512 || hash_secret(supplied) != hash_secret(csrf) {
        return Err(AuthError::Csrf);
    }
    if !single_write_header(headers, "idempotency-key").is_some_and(orange_protocol::valid_id) {
        return Err(AuthError::Invalid);
    }
    Ok(())
}

pub(crate) async fn database_time(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
        .fetch_one(&mut **tx)
        .await
}

pub(crate) fn renewed_idle_deadline(now: i64, idle: i64, absolute: i64) -> Option<i64> {
    (now < idle && now < absolute).then(|| now.saturating_add(900).min(absolute))
}
