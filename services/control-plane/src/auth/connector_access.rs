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

pub(super) async fn check_source(
    g: &mut UserSessionGuard,
    digest: &str,
) -> Result<(), UserApiError> {
    let tx = g.transaction();
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 649775))")
        .bind(digest)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let refused: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM connector_credential_faults WHERE source_digest=$1)",
    )
    .bind(digest)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if refused {
        return Err(UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "SOURCE_REGISTRATION_DISABLED",
        ));
    }
    Ok(())
}
pub(super) async fn disable_source(
    i: &UserIdentity,
    owner: &str,
    kind: &str,
    digest: &str,
) -> Result<(), UserApiError> {
    let mut tx = i.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,649775))")
        .bind(digest)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO connector_credential_faults(owner_id,connector_kind,source_digest) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(owner).bind(kind).bind(digest).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(())
}
pub(super) async fn disabled(
    g: &mut UserSessionGuard,
) -> Result<std::collections::BTreeSet<String>, UserApiError> {
    let owner = g.view().personal_owner_id.clone();
    let rows = sqlx::query_scalar(
        "SELECT source_digest FROM connector_credential_faults WHERE owner_id=$1",
    )
    .bind(owner)
    .fetch_all(&mut **g.transaction())
    .await
    .map_err(db)?;
    Ok(rows.into_iter().collect())
}

/// The resource grant and the action release are independent prerequisites.
pub(super) async fn released(
    g: &mut UserSessionGuard,
    provider: &str,
    grant: &str,
    action: &str,
) -> Result<(), UserApiError> {
    let pin = crate::connector_release::pin(provider, action).ok_or_else(denied)?;
    let ok: bool = sqlx::query_scalar("SELECT connector_grant_allows($1,$2,$3,$4)")
        .bind(provider)
        .bind(grant)
        .bind(action)
        .bind(pin)
        .fetch_one(&mut **g.transaction())
        .await
        .map_err(db)?;
    if !ok {
        return Err(denied());
    }
    Ok(())
}
pub(super) async fn released_action(
    g: &mut UserSessionGuard,
    provider: &str,
    grant: &str,
    action: &impl serde::Serialize,
) -> Result<(), UserApiError> {
    let body = serde_json::to_value(action).map_err(|_| AuthError::Invalid)?;
    released(
        g,
        provider,
        grant,
        body["action"].as_str().ok_or(AuthError::Invalid)?,
    )
    .await
}
