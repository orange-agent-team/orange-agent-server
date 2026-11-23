//! Verify the public device request before admitting it to the enrollment workflow.
use base64::{Engine, engine::general_purpose::STANDARD};
use orange_protocol::device_enrollment::WorkerCertificateRequest;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use x509_parser::{
    certification_request::X509CertificationRequest, extensions::ParsedExtension, prelude::FromDer,
};

pub(crate) fn request_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|v| {
        v.get_variant() == uuid::Variant::RFC4122 && v.get_version_num() == 4 && v.to_string() == id
    })
}

/// The returned DER contains only a proven client public key and the allowed request template.
pub(crate) fn verify(req: &WorkerCertificateRequest) -> Option<Vec<u8>> {
    if req.version != 1
        || req.purpose != "public_worker"
        || !request_id(&req.request_id)
        || !orange_protocol::valid_digest(&req.spki_sha256)
        || !orange_protocol::valid_digest(&req.csr_sha256)
        || req.csr_pem.len() > 4096
    {
        return None;
    }
    let pem = req.csr_pem.replace("\r\n", "\n");
    let body = pem
        .strip_prefix("-----BEGIN CERTIFICATE REQUEST-----\n")?
        .trim_end_matches('\n')
        .strip_suffix("\n-----END CERTIFICATE REQUEST-----")?;
    let bytes = STANDARD.decode(body.replace('\n', "")).ok()?;
    if hex::encode(Sha256::digest(&bytes)) != req.csr_sha256 {
        return None;
    }
    let (rest, csr) = X509CertificationRequest::from_der(&bytes).ok()?;
    let info = &csr.certification_request_info;
    let pk = &info.subject_pki;
    if !rest.is_empty()
        || info.version.0 != 0
        || info.attributes().len() != 1
        || pk.algorithm.algorithm.to_id_string() != "1.3.101.112"
        || pk.algorithm.parameters.is_some()
        || pk.subject_public_key.unused_bits != 0
        || pk.subject_public_key.data.len() != 32
        || csr.signature_algorithm != pk.algorithm
        || csr.signature_value.unused_bits != 0
        || csr.signature_value.data.len() != 64
        || hex::encode(Sha256::digest(pk.raw)) != req.spki_sha256
    {
        return None;
    }
    {
        let mut names = info.subject.iter_attributes();
        let cn = names.next()?;
        if names.next().is_some()
            || cn.attr_type().to_id_string() != "2.5.4.3"
            || cn.as_str().ok()? != format!("orange-worker-{}", req.request_id)
        {
            return None;
        }
    }
    let mut digital = false;
    let mut client = false;
    for ext in csr.requested_extensions()? {
        match ext {
            ParsedExtension::KeyUsage(k) if !digital && k.flags == 1 => digital = true,
            ParsedExtension::ExtendedKeyUsage(k)
                if !client
                    && k.client_auth
                    && !k.any
                    && !k.server_auth
                    && !k.code_signing
                    && !k.email_protection
                    && !k.time_stamping
                    && !k.ocsp_signing
                    && k.other.is_empty() =>
            {
                client = true
            }
            _ => return None,
        }
    }
    if !digital || !client {
        return None;
    }
    csr.verify_signature().ok()?;
    Some(bytes)
}
