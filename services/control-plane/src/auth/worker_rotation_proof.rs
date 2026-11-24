//! Bind the owner's session and the currently active leaf to the same replacement CSR.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls_pki_types::CertificateDer;
use sha2::{Digest, Sha256};
use x509_parser::pem::parse_x509_pem;

pub(super) fn decode_signature(handoff: &WorkerRotationHandoff) -> Result<Vec<u8>, UserApiError> {
    let encoded = &handoff.previous_key_signature;
    if encoded.len() != 86 {
        return Err(AuthError::Invalid.into());
    }
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AuthError::Invalid)?;
    if raw.len() != 64 || URL_SAFE_NO_PAD.encode(&raw) != *encoded {
        return Err(AuthError::Invalid.into());
    }
    Ok(raw)
}

pub(super) async fn bind(
    tx: &mut Tx<'_>,
    handoff: &WorkerRotationHandoff,
    digest: &str,
) -> Result<(), UserApiError> {
    let r = &handoff.intent;
    let signature = decode_signature(handoff)?;
    let unavailable = || UserApiError(StatusCode::CONFLICT, "ENROLLMENT_ROTATION_UNAVAILABLE");
    sqlx::query("SELECT lock_worker_issuer_policy()")
        .execute(&mut **tx)
        .await?;
    let row=sqlx::query("SELECT leaf_pem FROM renewable_worker_certificates WHERE node_id=$1 AND request_id=$2::text::uuid
        AND owner_id=$3 AND certificate_sha256=$4 AND issuer_sha256=$5 AND spki_sha256<>$6")
        .bind(&r.node_id).bind(&r.previous_request_id).bind(&r.owner_id).bind(&r.previous_certificate_sha256)
        .bind(&r.previous_issuer_sha256).bind(&r.replacement.request.spki_sha256)
        .fetch_optional(&mut **tx).await?.ok_or_else(unavailable)?;
    let pem: String = row.try_get("leaf_pem")?;
    let (rest, leaf) = parse_x509_pem(pem.as_bytes()).map_err(|_| unavailable())?;
    if !rest.iter().all(u8::is_ascii_whitespace)
        || leaf.label != "CERTIFICATE"
        || hex::encode(Sha256::digest(&leaf.contents)) != r.previous_certificate_sha256
    {
        return Err(unavailable());
    }
    let der = CertificateDer::from(leaf.contents);
    webpki::EndEntityCert::try_from(&der)
        .map_err(|_| unavailable())?
        .verify_signature(webpki::ring::ED25519, digest.as_bytes(), &signature)
        .map_err(|_| unavailable())?;
    sqlx::query("INSERT INTO worker_certificate_rotation_requests
        (request_id,node_id,previous_request_id,previous_certificate_sha256,intent_sha256,previous_key_signature)
        VALUES($1::text::uuid,$2,$3::text::uuid,$4,$5,$6)")
        .bind(&r.replacement.request.request_id).bind(&r.node_id).bind(&r.previous_request_id)
        .bind(&r.previous_certificate_sha256).bind(digest).bind(signature).execute(&mut **tx).await?;
    Ok(())
}
