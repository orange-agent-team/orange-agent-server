//! An enrollment token authorizes one enrollment receipt, never a user or worker control session.
use super::*;
use crate::worker::{enrollment_csr::request_id, enrollment_permissions::current_manager};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use orange_protocol::device_enrollment::{RedeemWorkerCertificate, WorkerCertificateReceipt};
use sqlx::Row;

fn invalid() -> UserApiError {
    UserApiError(StatusCode::UNAUTHORIZED, "ENROLLMENT_UNAVAILABLE")
}
fn commitment(headers: &HeaderMap) -> Result<String, UserApiError> {
    if headers.contains_key("cookie")
        || headers.contains_key("origin")
        || headers.get_all("authorization").iter().count() != 1
    {
        return Err(invalid());
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Orange-Enrollment "))
        .ok_or_else(invalid)?;
    if token.len() != 43 {
        return Err(invalid());
    }
    let raw = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
    if raw.len() != 32 || URL_SAFE_NO_PAD.encode(raw) != token {
        return Err(invalid());
    }
    Ok(crate::auth::hash_secret(token))
}
pub(super) async fn redeem(
    State(identity): State<Arc<UserIdentity>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<RedeemWorkerCertificate>, JsonRejection>,
) -> Result<Json<WorkerCertificateReceipt>, UserApiError> {
    let hash = commitment(&headers)?;
    let Json(body) = body.map_err(|_| AuthError::Invalid)?;
    if !request_id(&id)
        || body.version != 1
        || !request_id(&body.redemption_id)
        || headers.get_all("idempotency-key").iter().count() != 1
        || headers.get("idempotency-key").and_then(|h| h.to_str().ok())
            != Some(body.redemption_id.as_str())
    {
        return Err(AuthError::Invalid.into());
    }
    let mut tx = identity.pool.begin().await?;
    sqlx::query("SELECT lock_worker_issuer_policy()")
        .execute(&mut *tx)
        .await?;
    let grant=sqlx::query("SELECT owner_id,requested_by FROM worker_enrollment_grants WHERE request_id=$1::text::uuid AND exchange_token_sha256=$2
        AND state='authorized' AND expires_at>clock_timestamp()").bind(&id).bind(&hash).fetch_optional(&mut *tx).await?.ok_or_else(invalid)?;
    let owner: String = grant.try_get("owner_id")?;
    let user: String = grant.try_get("requested_by")?;
    if !current_manager(&mut tx, &owner, &user).await? {
        return Err(invalid());
    }
    let spki:String=sqlx::query_scalar("SELECT spki_sha256 FROM worker_enrollment_grants WHERE request_id=$1::text::uuid AND exchange_token_sha256=$2
        AND owner_id=$3 AND requested_by=$4 AND state='authorized' AND expires_at>clock_timestamp() FOR UPDATE")
        .bind(&id).bind(&hash).bind(&owner).bind(&user).fetch_optional(&mut *tx).await?.ok_or_else(invalid)?;
    let cert=sqlx::query("SELECT node_id,region_id,certificate_sha256,issuer_sha256,leaf_pem,floor(extract(epoch FROM expires_at))::bigint AS expiry,
        redemption_id::text,rotation_parent FROM worker_enrollment_certificates WHERE request_id=$1::text::uuid AND not_before<=clock_timestamp() AND expires_at>clock_timestamp() FOR UPDATE")
        .bind(&id).fetch_optional(&mut *tx).await?.ok_or(UserApiError(StatusCode::CONFLICT,"ENROLLMENT_CERTIFICATE_UNAVAILABLE"))?;
    if !sqlx::query_scalar::<_, bool>("SELECT worker_issued_leaf_allowed($1)")
        .bind(cert.try_get::<&str, _>("issuer_sha256")?)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(invalid());
    }
    let node: String = cert.try_get("node_id")?;
    let fingerprint: String = cert.try_get("certificate_sha256")?;
    if let Some(prior) = cert.try_get::<Option<String>, _>("redemption_id")? {
        if prior != body.redemption_id {
            return Err(UserApiError(
                StatusCode::CONFLICT,
                "ENROLLMENT_REDEMPTION_CONFLICT",
            ));
        }
    } else {
        if let Some(parent) = cert.try_get::<Option<String>, _>("rotation_parent")? {
            let locked=sqlx::query_scalar::<_,String>("SELECT node_id FROM nodes WHERE node_id=$1 AND owner_id=$2 AND region_id=$3 AND state NOT IN ('quarantined','revoked') FOR UPDATE")
                .bind(&node).bind(&owner).bind(cert.try_get::<&str,_>("region_id")?).fetch_optional(&mut *tx).await?;
            if locked.is_none() {
                return Err(invalid());
            }
            let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM renewable_worker_certificates WHERE certificate_sha256=$1 AND node_id=$2 AND owner_id=$3)")
                .bind(parent).bind(&node).bind(&owner).fetch_one(&mut *tx).await?;
            if !live {
                return Err(UserApiError(
                    StatusCode::CONFLICT,
                    "ENROLLMENT_ROTATION_UNAVAILABLE",
                ));
            }
        } else {
            sqlx::query("INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES($1,$2,'dev',$3,'offline')")
            .bind(&node).bind(&owner).bind(cert.try_get::<&str,_>("region_id")?).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO worker_credentials(certificate_sha256,node_id,enrollment_user_id,not_before,expires_at)
            SELECT certificate_sha256,node_id,$2,not_before,expires_at FROM worker_enrollment_certificates WHERE request_id=$1::text::uuid")
            .bind(&id).bind(&user).execute(&mut *tx).await?;
        sqlx::query("UPDATE worker_enrollment_certificates SET redemption_id=$2::text::uuid,redeemed_at=clock_timestamp() WHERE request_id=$1::text::uuid")
            .bind(&id).bind(&body.redemption_id).execute(&mut *tx).await.map_err(|e|if e.as_database_error().is_some_and(|d|d.is_unique_violation()){
                UserApiError(StatusCode::CONFLICT,"ENROLLMENT_REDEMPTION_CONFLICT")}else{e.into()})?;
    }
    let live=sqlx::query_scalar::<_,String>("SELECT c.certificate_sha256 FROM worker_credentials c JOIN nodes n USING(node_id)
        WHERE c.certificate_sha256=$1 AND c.node_id=$2 AND c.enrollment_user_id=$3 AND c.active AND n.owner_id=$4 AND n.state!='revoked' FOR SHARE OF c,n")
        .bind(&fingerprint).bind(&node).bind(&user).bind(&owner).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Err(invalid());
    }
    let timely:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM worker_enrollment_grants g JOIN worker_enrollment_certificates e USING(request_id)
        JOIN worker_credentials c USING(certificate_sha256) WHERE g.request_id=$1::text::uuid AND g.state='authorized' AND g.expires_at>clock_timestamp()
        AND c.active AND c.not_before<=clock_timestamp() AND c.expires_at>clock_timestamp() AND e.redemption_id=$2::text::uuid)")
        .bind(&id).bind(&body.redemption_id).fetch_one(&mut *tx).await?;
    if !timely {
        return Err(invalid());
    }
    let result = WorkerCertificateReceipt {
        version: 1,
        request_id: id,
        redemption_id: body.redemption_id,
        node_id: node,
        spki_sha256: spki,
        certificate_sha256: fingerprint,
        issuer_sha256: cert.try_get("issuer_sha256")?,
        expires_unix: cert.try_get::<i64, _>("expiry")?.to_string(),
        certificate_pem: cert.try_get("leaf_pem")?,
    };
    tx.commit().await?;
    Ok(Json(result))
}
