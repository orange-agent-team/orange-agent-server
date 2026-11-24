//! Prepare an offline rotation request using the private installed credential and an independent replacement key.
use super::*;
use orange_protocol::device_enrollment::{WorkerRotationHandoff, WorkerRotationIntent};
use rcgen::SigningKey;

pub fn prepare_rotation(
    config: &Path,
    owner: &str,
    new_id: &str,
) -> Result<WorkerRotationHandoff, Error> {
    let c: Config = serde_json::from_slice(&input(config, 8192)?).map_err(|_| Error::Config)?;
    if c.version != 1
        || c.environment != "development"
        || !orange_protocol::valid_id(owner)
        || !uuid4(new_id)
        || !uuid4(&c.request_id)
        || new_id == c.request_id
        || !orange_protocol::valid_digest(&c.issuer_sha256)
    {
        return Err(Error::Config);
    }
    // Local validation only. No redemption token or active network connection is required.
    let ca = verify::issuer(&input(&c.issuer_certificate, 32768)?, &c.issuer_sha256)?;
    let dir = open_directory(&c.device_directory)?;
    lock(&dir)?;
    let old = &c.request_id;
    let previous: Intent = decode(
        &read_at(&dir, format!("{old}.redemption.json").as_ref(), 4096)?.ok_or(Error::Local)?,
    )?;
    let receipt: WorkerCertificateReceipt = decode(
        &read_at(&dir, format!("{old}.receipt.json").as_ref(), 32768)?.ok_or(Error::Local)?,
    )?;
    let key = super::super::durable_key(&dir, old, false)?;
    if previous.version != 1
        || previous.request_id != *old
        || previous.origin != c.api_origin
        || previous.issuer_sha256 != c.issuer_sha256
        || previous.spki_sha256 != hash(&key.subject_public_key_info())
        || read_at(&dir, format!("{old}.client.pem").as_ref(), 16384)?.as_deref()
            != Some(receipt.certificate_pem.as_bytes())
    {
        return Err(Error::Conflict);
    }
    verify::leaf(&receipt, &previous, &ca)?;
    let record = format!("{new_id}.rotation.json");
    if let Some(raw) = read_at(&dir, record.as_ref(), 16384)? {
        let prior: WorkerRotationHandoff = decode(&raw)?;
        if prior.intent.owner_id != owner
            || prior.intent.previous_request_id != *old
            || read_at(&dir, format!("{new_id}.handoff.json").as_ref(), 8192)?.is_none()
            || read_at(&dir, format!("{new_id}.exchange-token").as_ref(), 46)?.is_none()
        {
            return Err(Error::Conflict);
        }
    }
    let replacement = prepare_handoff_locked(&dir, new_id)?;
    if replacement.request.spki_sha256 == previous.spki_sha256 {
        return Err(Error::Conflict);
    }
    let intent = WorkerRotationIntent {
        version: 1,
        owner_id: owner.to_owned(),
        node_id: receipt.node_id,
        previous_request_id: old.clone(),
        previous_certificate_sha256: receipt.certificate_sha256,
        previous_issuer_sha256: c.issuer_sha256,
        replacement,
    };
    let message = intent.signing_message().map_err(|_| Error::Config)?;
    let signature = key.sign(message.as_bytes()).map_err(|_| Error::Local)?;
    let handoff = WorkerRotationHandoff {
        intent,
        previous_key_signature: URL_SAFE_NO_PAD.encode(signature),
    };
    publish(&dir, &record, &encode(&handoff)?, 16384)?;
    Ok(handoff)
}

/// The saved handoff pins the expected stable node before sending a redemption. An already
/// confirmed replacement remains recoverable after the old credential has expired.
pub(super) fn bound_node(
    dir: &File,
    intent: &Intent,
    required: bool,
) -> Result<Option<String>, Error> {
    let Some(bytes) = read_at(
        dir,
        format!("{}.rotation.json", intent.request_id).as_ref(),
        16384,
    )?
    else {
        return if required {
            Err(Error::Conflict)
        } else {
            Ok(None)
        };
    };
    let handoff: WorkerRotationHandoff = decode(&bytes)?;
    let r = &handoff.intent;
    if r.version != 1
        || !uuid4(&r.previous_request_id)
        || !uuid4(&r.node_id)
        || !orange_protocol::valid_id(&r.owner_id)
        || r.previous_request_id == intent.request_id
        || r.replacement.request.request_id != intent.request_id
        || r.replacement.request.spki_sha256 != intent.spki_sha256
        || r.replacement.exchange_token_sha256 != intent.token_sha256
    {
        return Err(Error::Conflict);
    }
    let old: WorkerCertificateReceipt = decode(
        &read_at(
            dir,
            format!("{}.receipt.json", r.previous_request_id).as_ref(),
            32768,
        )?
        .ok_or(Error::Conflict)?,
    )?;
    if old.request_id != r.previous_request_id
        || old.node_id != r.node_id
        || old.issuer_sha256 != r.previous_issuer_sha256
        || old.certificate_sha256 != r.previous_certificate_sha256
    {
        return Err(Error::Conflict);
    }
    let der = verify::cert(old.certificate_pem.as_bytes())?;
    if hash(&der) != r.previous_certificate_sha256 {
        return Err(Error::Conflict);
    }
    let signature = URL_SAFE_NO_PAD
        .decode(&handoff.previous_key_signature)
        .map_err(|_| Error::Conflict)?;
    if signature.len() != 64 || URL_SAFE_NO_PAD.encode(&signature) != handoff.previous_key_signature
    {
        return Err(Error::Conflict);
    }
    webpki::EndEntityCert::try_from(&der)
        .map_err(|_| Error::Certificate)?
        .verify_signature(
            webpki::ring::ED25519,
            r.signing_message().map_err(|_| Error::Config)?.as_bytes(),
            &signature,
        )
        .map_err(|_| Error::Conflict)?;
    Ok(Some(r.node_id.clone()))
}
