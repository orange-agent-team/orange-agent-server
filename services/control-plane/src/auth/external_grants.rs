//! Shared durable issuance, revocation and completion across opted-in external source families.
use super::connector_access::{db, denied, missing};
use super::*;
use crate::auth::users::UserSessionGuard;
use sqlx::Row;
pub(super) struct Consent<'a> {
    pub run: &'a str,
    pub provider: &'static str,
    pub source: &'a str,
    pub source_digest: &'a str,
    pub roots: &'a [String],
    pub seconds: u32,
    pub key: &'a str,
    pub digest: &'a str,
}
impl Consent<'_> {
    pub(super) async fn issue(&self, g: &mut UserSessionGuard) -> Result<String, UserApiError> {
        let old=sqlx::query("SELECT grant_id,request_digest FROM external_source_grants WHERE run_id=$1 AND provider=$2 AND request_key=$3")
            .bind(self.run).bind(self.provider).bind(self.key).fetch_optional(&mut **g.transaction()).await.map_err(db)?;
        if let Some(row) = old {
            if row.get::<String, _>("request_digest") != self.digest {
                return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
            }
            return Ok(row.get("grant_id"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO external_source_grants(run_id,provider,grant_id,source_id,source_digest,request_key,request_digest,root_keys,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp()+make_interval(secs=>$9))")
            .bind(self.run).bind(self.provider).bind(&id).bind(self.source).bind(self.source_digest).bind(self.key).bind(self.digest).bind(self.roots).bind(f64::from(self.seconds)).execute(&mut **g.transaction()).await.map_err(db)?;
        Ok(id)
    }
}
pub(super) async fn revoke(
    g: &mut UserSessionGuard,
    run: &str,
    provider: &str,
    id: &str,
) -> Result<(), UserApiError> {
    sqlx::query("SELECT grant_id FROM external_source_grants WHERE run_id=$1 AND provider=$2 AND grant_id=$3").bind(run).bind(provider).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(missing)?;
    sqlx::query(
        "INSERT INTO repository_source_revocations(grant_id) VALUES($1) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .execute(&mut **g.transaction())
    .await
    .map_err(db)?;
    Ok(())
}
pub(super) async fn complete(
    g: &mut UserSessionGuard,
    run: &str,
    provider: &str,
    id: &str,
    key: &str,
) -> Result<(), UserApiError> {
    let updated=sqlx::query("UPDATE repository_source_calls SET finished_at=clock_timestamp() WHERE run_id=$1 AND provider=$2 AND grant_id=$3 AND request_key=$4 AND until_at>clock_timestamp() AND finished_at IS NULL")
        .bind(run).bind(provider).bind(id).bind(key).execute(&mut **g.transaction()).await.map_err(db)?;
    if updated.rows_affected() != 1 {
        return Err(denied());
    }
    Ok(())
}

pub(super) async fn document_opted(
    g: &mut UserSessionGuard,
    run: &str,
) -> Result<bool, UserApiError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT FROM cloud_document_source_opt_ins WHERE run_id=$1)")
        .bind(run)
        .fetch_one(&mut **g.transaction())
        .await
        .map_err(db)
}
