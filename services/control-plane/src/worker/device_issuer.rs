//! Dedicated offline development signer. The serving API and device never receive a CA private key.
use super::enrollment_csr;
use base64::{Engine, engine::general_purpose::STANDARD};
use orange_protocol::device_enrollment::WorkerCertificateRequest;
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData, SerialNumber,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;
use x509_parser::{parse_x509_certificate, pem::parse_x509_pem};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("private issuer configuration rejected")]
    Configuration,
    #[error("issuer certificate, key or validity rejected")]
    Issuer,
    #[error("device enrollment no longer authorized")]
    Unavailable,
    #[error("invalid device certificate request")]
    Request,
    #[error("could not persist the device certificate")]
    Storage(#[from] sqlx::Error),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    environment: String,
    region_id: String,
    ca_certificate_file: PathBuf,
    ca_key_file: PathBuf,
    ca_certificate_sha256: String,
}
struct Signer {
    issuer: Issuer<'static, KeyPair>,
    start: i64,
    end: i64,
    root: String,
    public_key_digest: String,
    region: String,
}
#[cfg(unix)]
fn read_private(path: &Path) -> Result<Vec<u8>, Error> {
    use std::os::unix::fs::MetadataExt;
    let parent = std::fs::symlink_metadata(path.parent().ok_or(Error::Configuration)?)
        .map_err(|_| Error::Configuration)?;
    if !path.is_absolute()
        || !parent.is_dir()
        || parent.uid() != rustix::process::geteuid().as_raw()
        || parent.mode() & 0o077 != 0
    {
        return Err(Error::Configuration);
    }
    crate::connector_file::read_private(path).map_err(|_| Error::Configuration)
}
impl Signer {
    #[cfg(unix)]
    fn load(path: &Path) -> Result<Self, Error> {
        let c: Config =
            serde_json::from_slice(&read_private(path)?).map_err(|_| Error::Configuration)?;
        if c.version != 1
            || c.environment != "development"
            || !orange_protocol::valid_id(&c.region_id)
            || !orange_protocol::valid_digest(&c.ca_certificate_sha256)
        {
            return Err(Error::Configuration);
        }
        let certificate = read_private(&c.ca_certificate_file)?;
        let key = read_private(&c.ca_key_file)?;
        let (trailing, pem) = parse_x509_pem(&certificate).map_err(|_| Error::Issuer)?;
        if !trailing.iter().all(u8::is_ascii_whitespace)
            || pem.label != "CERTIFICATE"
            || hex::encode(Sha256::digest(&pem.contents)) != c.ca_certificate_sha256
        {
            return Err(Error::Issuer);
        }
        let (tail, ca) = parse_x509_certificate(&pem.contents).map_err(|_| Error::Issuer)?;
        let pair = KeyPair::from_pem(std::str::from_utf8(&key).map_err(|_| Error::Issuer)?)
            .map_err(|_| Error::Issuer)?;
        if !tail.is_empty()
            || ca.subject() != ca.issuer()
            || ca.public_key().raw != pair.subject_public_key_info()
            || !ca
                .basic_constraints()
                .map_err(|_| Error::Issuer)?
                .is_some_and(|b| b.critical && b.value.ca)
            || !ca
                .key_usage()
                .map_err(|_| Error::Issuer)?
                .is_some_and(|k| k.value.key_cert_sign())
            || ca.verify_signature(None).is_err()
        {
            return Err(Error::Issuer);
        }
        let issuer = Issuer::from_ca_cert_der(&pem.contents.as_slice().into(), pair)
            .map_err(|_| Error::Issuer)?;
        Ok(Self {
            issuer,
            start: ca.validity().not_before.timestamp(),
            end: ca.validity().not_after.timestamp(),
            root: c.ca_certificate_sha256,
            public_key_digest: hex::encode(Sha256::digest(ca.public_key().raw)),
            region: c.region_id,
        })
    }
    fn make(
        &self,
        request: &WorkerCertificateRequest,
        now: i64,
    ) -> Result<(String, String, i64, i64), Error> {
        let der = enrollment_csr::verify(request).ok_or(Error::Request)?;
        let mut csr = CertificateSigningRequestParams::from_der(&der.as_slice().into())
            .map_err(|_| Error::Request)?;
        if now < self.start || self.end <= now + 60 {
            return Err(Error::Issuer);
        }
        let start = self.start.max(now - 60);
        let end = self.end.min(now + 86400);
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|_| Error::Issuer)?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(
            DnType::CommonName,
            format!("orange-worker-{}", request.request_id),
        );
        params.not_before =
            OffsetDateTime::from_unix_timestamp(start).map_err(|_| Error::Issuer)?;
        params.not_after = OffsetDateTime::from_unix_timestamp(end).map_err(|_| Error::Issuer)?;
        params.serial_number = Some(SerialNumber::from_slice(Uuid::new_v4().as_bytes()));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        csr.params = params;
        let certificate = csr.signed_by(&self.issuer).map_err(|_| Error::Issuer)?;
        Ok((
            certificate.pem(),
            hex::encode(Sha256::digest(certificate.der().as_ref())),
            start,
            end,
        ))
    }
}
#[cfg(unix)]
pub async fn issue_one(pool: &PgPool, path: &Path) -> Result<Option<String>, Error> {
    let signer = Signer::load(path)?;
    // The configured CA and its private key were matched locally. The database receives only public digests.
    let mut registration = pool.begin().await?;
    sqlx::query("SELECT lock_worker_issuer_policy()")
        .execute(&mut *registration)
        .await?;
    if !sqlx::query_scalar::<_, bool>("SELECT worker_trust_path_allowed(ARRAY[$1::text])")
        .bind(&signer.public_key_digest)
        .fetch_one(&mut *registration)
        .await?
    {
        return Err(Error::Issuer);
    }
    let valid:bool=sqlx::query_scalar("SELECT clock_timestamp()>=to_timestamp($1::bigint) AND clock_timestamp()<to_timestamp($2::bigint)")
        .bind(signer.start).bind(signer.end).fetch_one(&mut *registration).await?;
    if !valid {
        return Err(Error::Issuer);
    }
    sqlx::query("INSERT INTO worker_issuer_public_keys(issuer_sha256,spki_sha256) VALUES($1,$2) ON CONFLICT(issuer_sha256) DO NOTHING")
        .bind(&signer.root).bind(&signer.public_key_digest).execute(&mut *registration).await?;
    let observed: String = sqlx::query_scalar(
        "SELECT spki_sha256 FROM worker_issuer_public_keys WHERE issuer_sha256=$1",
    )
    .bind(&signer.root)
    .fetch_one(&mut *registration)
    .await?;
    if observed != signer.public_key_digest {
        return Err(Error::Issuer);
    }
    registration.commit().await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT lock_worker_issuer_policy()")
        .execute(&mut *tx)
        .await?;
    if !sqlx::query_scalar::<_, bool>("SELECT worker_issued_leaf_allowed($1)")
        .bind(&signer.root)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(Error::Issuer);
    }
    let candidate=sqlx::query("SELECT g.request_id::text,g.owner_id,g.requested_by FROM worker_enrollment_grants g
        JOIN users u ON u.user_id=g.requested_by AND u.active JOIN owners personal ON personal.owner_id=u.personal_owner_id AND personal.state='active' JOIN owners o ON o.owner_id=g.owner_id AND o.state='active'
        WHERE g.state='authorized' AND g.expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM worker_enrollment_certificates c WHERE c.request_id=g.request_id)
        AND (NOT EXISTS(SELECT FROM worker_certificate_rotation_requests r WHERE r.request_id=g.request_id)
          OR EXISTS(SELECT FROM worker_certificate_rotation_requests r JOIN renewable_worker_certificates src
                   ON src.certificate_sha256=r.previous_certificate_sha256 AND src.request_id=r.previous_request_id
                   WHERE r.request_id=g.request_id AND r.node_id=src.node_id AND src.owner_id=g.owner_id AND src.region_id=$1))
        AND (u.personal_owner_id=g.owner_id OR EXISTS(SELECT FROM memberships m WHERE m.organization_id=g.owner_id AND m.user_id=u.user_id AND m.active AND m.role IN ('owner','admin')))
        ORDER BY g.created_at,g.request_id LIMIT 1").bind(&signer.region).fetch_optional(&mut *tx).await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let id: String = candidate.try_get("request_id")?;
    let owner: String = candidate.try_get("owner_id")?;
    let user: String = candidate.try_get("requested_by")?;
    if !super::enrollment_permissions::current_manager(&mut tx, &owner, &user).await? {
        return Ok(None);
    }
    let row=sqlx::query("SELECT csr_der,csr_sha256,spki_sha256 FROM worker_enrollment_grants WHERE request_id=$1::text::uuid
        AND owner_id=$2 AND requested_by=$3 AND state='authorized' AND expires_at>clock_timestamp() FOR UPDATE SKIP LOCKED")
        .bind(&id).bind(&owner).bind(&user).fetch_optional(&mut *tx).await?;
    let Some(r) = row else { return Ok(None) };
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM worker_enrollment_certificates WHERE request_id=$1::text::uuid)",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Ok(None);
    }
    let bytes: Vec<u8> = r.try_get("csr_der")?;
    let request = WorkerCertificateRequest {
        version: 1,
        purpose: "public_worker".into(),
        request_id: id.clone(),
        spki_sha256: r.try_get("spki_sha256")?,
        csr_sha256: r.try_get("csr_sha256")?,
        csr_pem: format!(
            "-----BEGIN CERTIFICATE REQUEST-----\n{}\n-----END CERTIFICATE REQUEST-----\n",
            STANDARD.encode(bytes)
        ),
    };
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
            .fetch_one(&mut *tx)
            .await?;
    let (pem, fingerprint, start, end) = signer.make(&request, now)?;
    let source:Option<(String,String)>=sqlx::query_as("SELECT node_id,previous_certificate_sha256 FROM worker_certificate_rotation_requests WHERE request_id=$1::text::uuid")
        .bind(&id).fetch_optional(&mut *tx).await?;
    let (node, parent) = match source {
        Some((node, parent)) => (node, Some(parent)),
        None => (Uuid::new_v4().to_string(), None),
    };
    let changed=sqlx::query("INSERT INTO worker_enrollment_certificates(request_id,node_id,region_id,certificate_sha256,issuer_sha256,leaf_pem,not_before,expires_at,rotation_parent)
        SELECT request_id,$2,$3,$4,$5,$6,to_timestamp($7::bigint),to_timestamp($8::bigint),$9 FROM worker_enrollment_grants
        WHERE request_id=$1::text::uuid AND state='authorized' AND expires_at>clock_timestamp() AND clock_timestamp()<to_timestamp($8::bigint)")
        .bind(&id).bind(&node).bind(signer.region).bind(fingerprint).bind(signer.root).bind(pem).bind(start).bind(end).bind(parent).execute(&mut *tx).await?.rows_affected();
    if changed != 1 {
        return Err(Error::Unavailable);
    }
    tx.commit().await?;
    Ok(Some(id))
}
