use super::*;
use crate::auth::users::UserSessionGuard;
pub(super) fn db(_: sqlx::Error) -> UserApiError {
    AuthError::Unavailable.into()
}
pub(super) fn missing() -> UserApiError {
    UserApiError(StatusCode::NOT_FOUND, "NOT_FOUND")
}
pub(super) fn denied() -> UserApiError {
    UserApiError(StatusCode::FORBIDDEN, "CONNECTOR_AUTHORITY_REQUIRED")
}
pub(super) fn key(headers: &HeaderMap) -> Result<&str, UserApiError> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| valid_id(s))
        .ok_or(AuthError::Invalid.into())
}
pub(super) async fn guard(
    identity: &UserIdentity,
    headers: &HeaderMap,
    run: &str,
    write: bool,
    active: bool,
) -> Result<(UserSessionGuard, bool), UserApiError> {
    let mut g = identity.authenticate(headers).await?;
    if write {
        check_write(headers, &identity.config.user_origin, &g.view().csrf_token)?;
    }
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let live:bool=sqlx::query_scalar("SELECT r.status IN ('running','paused','waiting_approval') AND a.state='available' FROM cloud_runs r JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 FOR UPDATE OF r FOR SHARE OF a")
        .bind(run).bind(owner).bind(user).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(missing)?;
    if active && !live {
        return Err(denied());
    }
    Ok((g, live))
}
pub(super) async fn capacity(g: &mut UserSessionGuard, run: &str) -> Result<(), UserApiError> {
    let available:bool=sqlx::query_scalar("SELECT count(*)<100 AND count(*) FILTER(WHERE finished_at IS NULL AND until_at>clock_timestamp())<4 FROM connector_run_calls WHERE run_id=$1").bind(run).fetch_one(&mut **g.transaction()).await.map_err(db)?;
    if !available {
        return Err(UserApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "CONNECTOR_LIMIT",
        ));
    }
    Ok(())
}
