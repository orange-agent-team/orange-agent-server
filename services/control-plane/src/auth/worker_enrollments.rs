//! A browser manager authorizes a verified public key request. No CA key lives in this router.
#[path = "worker_rotation_proof.rs"]
mod rotation_proof;
use super::*;
use crate::auth::users::UserSessionGuard;
use crate::worker::enrollment_csr;
use orange_protocol::device_enrollment::WorkerRotationHandoff;
use orange_protocol::{
    device_enrollment::{AuthorizeWorkerEnrollment, WorkerEnrollmentView},
    team::TeamRole,
};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};

type Tx<'a> = Transaction<'a, Postgres>;
macro_rules! enrollment_select { () => { "SELECT request_id::text,owner_id,requested_by,spki_sha256,csr_sha256,exchange_token_sha256,
    CASE WHEN state='authorized' AND expires_at<=clock_timestamp() THEN 'expired' WHEN state='authorized' AND EXISTS(SELECT FROM worker_enrollment_certificates c WHERE c.request_id=g.request_id AND c.redeemed_at IS NOT NULL) THEN 'redeemed' WHEN state='authorized' AND EXISTS(SELECT FROM worker_enrollment_certificates c WHERE c.request_id=g.request_id) THEN 'certificate_prepared' ELSE state END AS visible_state,
    floor(extract(epoch FROM expires_at))::bigint AS expires FROM worker_enrollment_grants g WHERE owner_id=$1 AND request_id=$2::text::uuid" }; }
const SELECT: &str = enrollment_select!();

async fn manager(
    identity: &UserIdentity,
    headers: &HeaderMap,
    owner: &str,
    id: &str,
    write: bool,
) -> Result<UserSessionGuard, UserApiError> {
    let mut guard = identity.authenticate(headers).await?;
    if !valid_id(owner) || !enrollment_csr::request_id(id) {
        return Err(AuthError::Invalid.into());
    }
    if write {
        check_write(
            headers,
            &identity.config.user_origin,
            &guard.view().csrf_token,
        )?;
        if headers.get_all("idempotency-key").iter().count() != 1
            || headers.get("idempotency-key").and_then(|h| h.to_str().ok()) != Some(id)
        {
            return Err(AuthError::Invalid.into());
        }
    }
    if owner != guard.view().personal_owner_id {
        let user = guard.view().user_id.clone();
        crate::team::organization_row(guard.transaction(), owner, false).await?;
        let role = crate::team::membership(guard.transaction(), owner, &user).await?;
        if !matches!(role, TeamRole::Owner | TeamRole::Admin) {
            return Err(TeamError::Forbidden.into());
        }
    }
    Ok(guard)
}
fn view(row: &PgRow) -> Result<WorkerEnrollmentView, UserApiError> {
    Ok(WorkerEnrollmentView {
        request_id: row.try_get("request_id")?,
        owner_id: row.try_get("owner_id")?,
        requested_by: row.try_get("requested_by")?,
        spki_sha256: row.try_get("spki_sha256")?,
        csr_sha256: row.try_get("csr_sha256")?,
        expires_unix: row.try_get::<i64, _>("expires")?.to_string(),
        state: row.try_get("visible_state")?,
    })
}
async fn read(tx: &mut Tx<'_>, owner: &str, id: &str) -> Result<Option<PgRow>, sqlx::Error> {
    sqlx::query(SELECT)
        .bind(owner)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
}
async fn serialize(tx: &mut Tx<'_>, owner: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('worker-enrollment:' || $1,37))")
        .bind(owner)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub(super) async fn authorize(
    State(identity): State<Arc<UserIdentity>>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<AuthorizeWorkerEnrollment>, JsonRejection>,
) -> Result<Json<WorkerEnrollmentView>, UserApiError> {
    let Json(input) = body.map_err(|_| AuthError::Invalid)?;
    authorize_request(identity, owner, id, headers, &input, None).await
}
pub(super) async fn authorize_rotation(
    State(identity): State<Arc<UserIdentity>>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<WorkerRotationHandoff>, JsonRejection>,
) -> Result<Json<WorkerEnrollmentView>, UserApiError> {
    let Json(handoff) = body.map_err(|_| AuthError::Invalid)?;
    if handoff.intent.version != 1
        || handoff.intent.owner_id != owner
        || handoff.intent.replacement.request.request_id != id
        || !crate::worker::enrollment_csr::request_id(&handoff.intent.node_id)
        || !crate::worker::enrollment_csr::request_id(&handoff.intent.previous_request_id)
        || handoff.intent.previous_request_id == id
        || !orange_protocol::valid_digest(&handoff.intent.previous_certificate_sha256)
        || !orange_protocol::valid_digest(&handoff.intent.previous_issuer_sha256)
    {
        return Err(AuthError::Invalid.into());
    }
    authorize_request(
        identity,
        owner,
        id,
        headers,
        &handoff.intent.replacement,
        Some(&handoff),
    )
    .await
}
async fn authorize_request(
    identity: Arc<UserIdentity>,
    owner: String,
    id: String,
    headers: HeaderMap,
    input: &AuthorizeWorkerEnrollment,
    rotation: Option<&WorkerRotationHandoff>,
) -> Result<Json<WorkerEnrollmentView>, UserApiError> {
    let mut guard = manager(&identity, &headers, &owner, &id, true).await?;
    let req = &input.request;
    if req.request_id != id
        || !orange_protocol::valid_digest(&input.exchange_token_sha256)
        || input.exchange_token_sha256 == req.csr_sha256
        || input.exchange_token_sha256 == req.spki_sha256
        || input.exchange_token_sha256 == "0".repeat(64)
    {
        return Err(AuthError::Invalid.into());
    }
    let der = enrollment_csr::verify(req).ok_or(AuthError::Invalid)?;
    let user = guard.view().user_id.clone();
    let tx = guard.transaction();
    serialize(tx, &owner).await?;
    let conflict = || UserApiError(StatusCode::CONFLICT, "ENROLLMENT_CONFLICT");
    let proposed = rotation
        .map(|r| r.intent.signing_message())
        .transpose()
        .map_err(|_| AuthError::Invalid)?;
    if let Some(row) = read(tx, &owner, &id).await? {
        let stored=sqlx::query_scalar::<_,String>("SELECT intent_sha256 FROM worker_certificate_rotation_requests WHERE request_id=$1::text::uuid")
            .bind(&id).fetch_optional(&mut **tx).await?;
        if stored != proposed {
            return Err(conflict());
        }
        if let Some(handoff) = rotation {
            // A retry with a copied digest but a different signature is not the original authorization.
            let signature = rotation_proof::decode_signature(handoff)?;
            let bound:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM worker_certificate_rotation_requests WHERE request_id=$1::text::uuid AND previous_key_signature=$2)")
                .bind(&id).bind(signature).fetch_one(&mut **tx).await?;
            if !bound {
                return Err(conflict());
            }
        }
        if row.try_get::<&str, _>("requested_by")? != user
            || row.try_get::<&str, _>("spki_sha256")? != req.spki_sha256
            || row.try_get::<&str, _>("csr_sha256")? != req.csr_sha256
            || row.try_get::<&str, _>("exchange_token_sha256")? != input.exchange_token_sha256
        {
            return Err(conflict());
        }
    } else {
        let n:i64=sqlx::query_scalar("SELECT count(*) FROM worker_enrollment_grants WHERE owner_id=$1 AND created_at>clock_timestamp()-interval '1 hour'")
            .bind(&owner).fetch_one(&mut **tx).await?;
        if n >= 24 {
            return Err(UserApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "ENROLLMENT_RATE_LIMIT",
            ));
        }
        let inserted=sqlx::query("WITH t AS MATERIALIZED (SELECT clock_timestamp() AS n) INSERT INTO worker_enrollment_grants
            (request_id,owner_id,requested_by,spki_sha256,csr_sha256,csr_der,exchange_token_sha256,created_at,expires_at)
            SELECT $1::text::uuid,$2,$3,$4,$5,$6,$7,n,n+interval '10 minutes' FROM t ON CONFLICT DO NOTHING")
            .bind(&id).bind(&owner).bind(&user).bind(&req.spki_sha256).bind(&req.csr_sha256).bind(&der).bind(&input.exchange_token_sha256)
            .execute(&mut **tx).await?.rows_affected();
        if inserted != 1 {
            return Err(conflict());
        }
        if let (Some(handoff), Some(digest)) = (rotation, proposed) {
            rotation_proof::bind(tx, handoff, &digest).await?;
        }
    }
    let result = view(&read(tx, &owner, &id).await?.ok_or_else(conflict)?)?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn inspect_rotation(
    State(identity): State<Arc<UserIdentity>>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<orange_protocol::device_enrollment::WorkerRotationView>, UserApiError> {
    let mut guard = manager(&identity, &headers, &owner, &id, false).await?;
    let statement=concat!("WITH enrollment AS (",enrollment_select!(),") SELECT enrollment.*,r.node_id,r.previous_request_id::text,r.previous_certificate_sha256,r.intent_sha256,
        EXISTS(SELECT FROM renewable_worker_certificates src WHERE src.certificate_sha256=r.previous_certificate_sha256 AND src.node_id=r.node_id AND src.owner_id=enrollment.owner_id) AS eligible,
        floor(extract(epoch FROM a.activated_at))::bigint AS activated,floor(extract(epoch FROM a.overlap_until))::bigint AS cutoff
        FROM enrollment JOIN worker_certificate_rotation_requests r ON r.request_id=enrollment.request_id::uuid
        LEFT JOIN worker_certificate_rotation_receipts a ON a.request_id=r.request_id");
    let row = sqlx::query(statement)
        .bind(&owner)
        .bind(&id)
        .fetch_optional(&mut **guard.transaction())
        .await?
        .ok_or(UserApiError(
            StatusCode::NOT_FOUND,
            "ENROLLMENT_ROTATION_NOT_FOUND",
        ))?;
    let result = orange_protocol::device_enrollment::WorkerRotationView {
        enrollment: view(&row)?,
        node_id: row.try_get("node_id")?,
        previous_request_id: row.try_get("previous_request_id")?,
        previous_certificate_sha256: row.try_get("previous_certificate_sha256")?,
        intent_sha256: row.try_get("intent_sha256")?,
        predecessor_eligible: row.try_get("eligible")?,
        activated_unix: row
            .try_get::<Option<i64>, _>("activated")?
            .map(|v| v.to_string()),
        overlap_until_unix: row
            .try_get::<Option<i64>, _>("cutoff")?
            .map(|v| v.to_string()),
    };
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn inspect(
    State(identity): State<Arc<UserIdentity>>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<WorkerEnrollmentView>, UserApiError> {
    let mut guard = manager(&identity, &headers, &owner, &id, false).await?;
    let result = view(
        &read(guard.transaction(), &owner, &id)
            .await?
            .ok_or(UserApiError(StatusCode::NOT_FOUND, "ENROLLMENT_NOT_FOUND"))?,
    )?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn revoke(
    State(identity): State<Arc<UserIdentity>>,
    Path((owner, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<WorkerEnrollmentView>, UserApiError> {
    let mut guard = manager(&identity, &headers, &owner, &id, true).await?;
    let tx = guard.transaction();
    serialize(tx, &owner).await?;
    sqlx::query("UPDATE worker_enrollment_grants SET state='revoked',revoked_at=clock_timestamp() WHERE owner_id=$1 AND request_id=$2::text::uuid AND state='authorized'")
        .bind(&owner).bind(&id).execute(&mut **tx).await?;
    let result = view(
        &read(tx, &owner, &id)
            .await?
            .ok_or(UserApiError(StatusCode::NOT_FOUND, "ENROLLMENT_NOT_FOUND"))?,
    )?;
    guard.finish().await?;
    Ok(Json(result))
}
