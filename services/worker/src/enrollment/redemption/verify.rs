use super::{Error, Intent, WorkerCertificateReceipt, hash, uuid4};
use rustls_pki_types::{CertificateDer, UnixTime};
use x509_parser::{parse_x509_certificate, pem::parse_x509_pem};

pub(super) fn cert(bytes: &[u8]) -> Result<CertificateDer<'static>, Error> {
    if !bytes.starts_with(b"-----BEGIN CERTIFICATE-----") {
        return Err(Error::Certificate);
    }
    let (rest, pem) = parse_x509_pem(bytes).map_err(|_| Error::Certificate)?;
    if !rest.iter().all(u8::is_ascii_whitespace) || pem.label != "CERTIFICATE" {
        return Err(Error::Certificate);
    }
    Ok(pem.contents.into())
}

pub(super) fn issuer(bytes: &[u8], pin: &str) -> Result<CertificateDer<'static>, Error> {
    let der = cert(bytes)?;
    let (rest, root) = parse_x509_certificate(&der).map_err(|_| Error::Certificate)?;
    if !rest.is_empty()
        || hash(&der) != pin
        || root.subject() != root.issuer()
        || !root.validity().is_valid()
        || root.verify_signature(None).is_err()
        || !root
            .basic_constraints()
            .map_err(|_| Error::Certificate)?
            .is_some_and(|v| v.critical && v.value.ca)
        || !root
            .key_usage()
            .map_err(|_| Error::Certificate)?
            .is_some_and(|v| v.value.key_cert_sign())
    {
        return Err(Error::Certificate);
    }
    Ok(der)
}

pub(super) fn leaf(
    receipt: &WorkerCertificateReceipt,
    intent: &Intent,
    ca: &CertificateDer<'_>,
) -> Result<(), Error> {
    if receipt.version != 1
        || receipt.request_id != intent.request_id
        || receipt.redemption_id != intent.redemption_id
        || !uuid4(&receipt.node_id)
        || receipt.spki_sha256 != intent.spki_sha256
        || receipt.issuer_sha256 != intent.issuer_sha256
        || receipt.certificate_pem.len() > 16384
    {
        return Err(Error::Certificate);
    }
    let der = cert(receipt.certificate_pem.as_bytes())?;
    if hash(&der) != receipt.certificate_sha256 {
        return Err(Error::Certificate);
    }
    let (_, root) = parse_x509_certificate(ca).map_err(|_| Error::Certificate)?;
    let (rest, c) = parse_x509_certificate(&der).map_err(|_| Error::Certificate)?;
    let start = c.validity().not_before.timestamp();
    let end = c.validity().not_after.timestamp();
    let mut names = c.subject().iter_attributes();
    let subject = names.next().ok_or(Error::Certificate)?;
    if !rest.is_empty()
        || names.next().is_some()
        || subject.attr_type().to_id_string() != "2.5.4.3"
        || subject.as_str().map_err(|_| Error::Certificate)?
            != format!("orange-worker-{}", intent.request_id)
        || c.issuer() != root.subject()
        || c.subject_alternative_name()
            .map_err(|_| Error::Certificate)?
            .is_some()
        || c.basic_constraints()
            .map_err(|_| Error::Certificate)?
            .is_some_and(|v| v.value.ca)
        || !c
            .key_usage()
            .map_err(|_| Error::Certificate)?
            .is_some_and(|v| v.value.flags == 1)
        || !c
            .extended_key_usage()
            .map_err(|_| Error::Certificate)?
            .is_some_and(|v| {
                let k = v.value;
                k.client_auth
                    && !k.any
                    && !k.server_auth
                    && !k.code_signing
                    && !k.email_protection
                    && !k.time_stamping
                    && !k.ocsp_signing
                    && k.other.is_empty()
            })
        || hash(c.public_key().raw) != intent.spki_sha256
        || end <= start
        || end - start > 86460
        || start < root.validity().not_before.timestamp()
        || end > root.validity().not_after.timestamp()
        || end.to_string() != receipt.expires_unix
    {
        return Err(Error::Certificate);
    }
    let anchor = webpki::anchor_from_trusted_cert(ca).map_err(|_| Error::Certificate)?;
    webpki::EndEntityCert::try_from(&der)
        .map_err(|_| Error::Certificate)?
        .verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            &[anchor],
            &[],
            UnixTime::now(),
            webpki::KeyUsage::client_auth(),
            None,
            None,
        )
        .map_err(|_| Error::Certificate)?;
    Ok(())
}
