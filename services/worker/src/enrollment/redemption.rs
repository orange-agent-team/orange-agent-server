//! Redeem an existing device key. Commit the replay identity before sending the one-time token.
use super::{Error as KeyError, lock, open_directory};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use orange_protocol::device_enrollment::{RedeemWorkerCertificate, WorkerCertificateReceipt};
use rcgen::PublicKeyData;
use reqwest::{Client, Url, redirect::Policy};
use rustix::fs::{self, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

mod rotation;
mod verify;
pub use rotation::prepare_rotation;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid private enrollment configuration or token")]
    Config,
    #[error("enrollment is already bound to different local inputs")]
    Conflict,
    #[error("device key or private storage is unavailable")]
    Local,
    #[error("certificate response failed identity or issuer verification")]
    Certificate,
    #[error("enrollment service refused the token or grant")]
    Denied,
    #[error("certificate unavailable or enrollment redemption conflict")]
    Unavailable,
    #[error("redemption outcome unknown; retry with the same configuration")]
    Unknown,
}
impl From<KeyError> for Error {
    fn from(_: KeyError) -> Self {
        Self::Local
    }
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Local
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(_: rustix::io::Errno) -> Self {
        Self::Local
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    environment: String,
    api_origin: String,
    api_tls_ca_file: Option<PathBuf>,
    device_directory: PathBuf,
    request_id: String,
    exchange_token_file: PathBuf,
    issuer_certificate: PathBuf,
    issuer_sha256: String,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    request_id: String,
    redemption_id: String,
    origin: String,
    spki_sha256: String,
    issuer_sha256: String,
    token_sha256: String,
    api_ca_sha256: Option<String>,
}
#[derive(Serialize)]
pub struct Installation {
    version: u32,
    request_id: String,
    redemption_id: String,
    node_id: String,
    spki_sha256: String,
    certificate_sha256: String,
    certificate: PathBuf,
    private_key: PathBuf,
    expires_unix: String,
    source: &'static str,
}
fn uuid4(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|u| {
        u.get_variant() == uuid::Variant::RFC4122 && u.get_version_num() == 4 && u.to_string() == s
    })
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn flags() -> OFlags {
    OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW
}

fn file_ok(file: &File, limit: u64) -> Result<(), Error> {
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
        || m.len() > limit
    {
        return Err(Error::Local);
    }
    Ok(())
}
fn read_at(dir: &File, name: &std::ffi::OsStr, limit: u64) -> Result<Option<Vec<u8>>, Error> {
    let fd = match fs::openat(dir, name, OFlags::RDONLY | flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let f = File::from(fd);
    file_ok(&f, limit)?;
    let before = f.metadata()?;
    let mut bytes = Vec::new();
    (&f).take(limit + 1).read_to_end(&mut bytes)?;
    let after = f.metadata()?;
    if bytes.len() as u64 != before.len()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(Error::Local);
    }
    Ok(Some(bytes))
}
fn input(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let dir = open_directory(path.parent().ok_or(Error::Config)?)?;
    read_at(&dir, path.file_name().ok_or(Error::Config)?, limit)?.ok_or(Error::Config)
}
/// The caller holds the private directory lock. A torn, unpublished staging file is safe to replace.
fn publish(dir: &File, name: &str, bytes: &[u8], limit: u64) -> Result<(), Error> {
    if bytes.len() as u64 > limit {
        return Err(Error::Local);
    }
    if let Some(prior) = read_at(dir, name.as_ref(), limit)? {
        if prior != bytes {
            return Err(Error::Conflict);
        }
        dir.sync_all()?;
        return Ok(());
    }
    let pending = format!(".{name}.pending");
    let mut file = File::from(fs::openat(
        dir,
        pending.as_str(),
        OFlags::RDWR | OFlags::CREATE | flags(),
        Mode::RUSR | Mode::WUSR,
    )?);
    file_ok(&file, limit)?;
    file.rewind()?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::renameat_with(dir, pending.as_str(), dir, name, RenameFlags::NOREPLACE)?;
    dir.sync_all()?;
    Ok(())
}
fn spki(dir: &File, id: &str) -> Result<String, Error> {
    Ok(hash(
        &super::durable_key(dir, id, false)?.subject_public_key_info(),
    ))
}
fn client(c: &Config) -> Result<(Client, Option<String>), Error> {
    let url = Url::parse(&c.api_origin).map_err(|_| Error::Config)?;
    if url.origin().ascii_serialization() != c.api_origin
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
    {
        return Err(Error::Config);
    }
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8));
    let pin = match (url.scheme(), &c.api_tls_ca_file) {
        ("http", None) if url.host_str() != Some("localhost") => None,
        ("https", Some(path)) => {
            let bytes = input(path, 32768)?;
            let cert = reqwest::Certificate::from_pem(&bytes).map_err(|_| Error::Config)?;
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(cert);
            Some(hash(&bytes))
        }
        _ => return Err(Error::Config),
    };
    Ok((builder.build().map_err(|_| Error::Config)?, pin))
}
fn parse_token(bytes: &[u8]) -> Result<String, Error> {
    let s = std::str::from_utf8(bytes).map_err(|_| Error::Config)?;
    let s = s
        .strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s);
    let decoded = URL_SAFE_NO_PAD.decode(s).map_err(|_| Error::Config)?;
    if s.len() != 43 || decoded.len() != 32 || URL_SAFE_NO_PAD.encode(decoded) != s {
        return Err(Error::Config);
    }
    Ok(s.to_owned())
}
fn decode<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(raw).map_err(|_| Error::Conflict)
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(value).map_err(|_| Error::Local)
}
async fn exchange(
    c: &Config,
    client: Client,
    intent: &Intent,
    token: &str,
) -> Result<WorkerCertificateReceipt, Error> {
    let url = format!(
        "{}/api/v1/worker-enrollments/{}/redeem",
        c.api_origin, c.request_id
    );
    let mut response = client
        .post(url)
        .header("authorization", format!("Orange-Enrollment {token}"))
        .header("idempotency-key", &intent.redemption_id)
        .json(&RedeemWorkerCertificate {
            version: 1,
            redemption_id: intent.redemption_id.clone(),
        })
        .send()
        .await
        .map_err(|_| Error::Unknown)?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(Error::Denied),
        409 => return Err(Error::Unavailable),
        _ => return Err(Error::Unknown),
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unknown)? {
        if chunk.len() > 32768 - bytes.len() {
            return Err(Error::Certificate);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Certificate)
}
/// The owner's browser receives only the public request and a commitment to the device's secret.
pub fn prepare_handoff(
    path: &Path,
    id: &str,
) -> Result<orange_protocol::device_enrollment::AuthorizeWorkerEnrollment, Error> {
    super::validate_id(id)?;
    let dir = open_directory(path)?;
    lock(&dir)?;
    prepare_handoff_locked(&dir, id)
}
fn prepare_handoff_locked(
    dir: &File,
    id: &str,
) -> Result<orange_protocol::device_enrollment::AuthorizeWorkerEnrollment, Error> {
    use orange_protocol::device_enrollment::AuthorizeWorkerEnrollment;
    let record = format!("{id}.handoff.json");
    let token_name = format!("{id}.exchange-token");
    let prior = read_at(dir, record.as_ref(), 8192)?;
    let intent = read_at(dir, format!("{id}.redemption.json").as_ref(), 4096)?;
    let initialized = prior.is_some() || intent.is_some();
    let key = super::durable_key(dir, id, !initialized)?;
    let request = super::request(&key, id)?;
    let secret = match read_at(dir, token_name.as_ref(), 46)? {
        Some(raw) => parse_token(&raw)?,
        None if !initialized => {
            let mut random = [0u8; 32];
            getrandom::fill(&mut random).map_err(|_| Error::Local)?;
            let secret = URL_SAFE_NO_PAD.encode(random);
            publish(dir, &token_name, secret.as_bytes(), 46)?;
            secret
        }
        None => return Err(Error::Conflict),
    };
    let hash = hash(secret.as_bytes());
    if let Some(raw) = intent {
        let i: Intent = decode(&raw)?;
        if i.request_id != id || i.spki_sha256 != request.spki_sha256 || i.token_sha256 != hash {
            return Err(Error::Conflict);
        }
    }
    let handoff = AuthorizeWorkerEnrollment {
        request,
        exchange_token_sha256: hash,
    };
    let bytes = encode(&handoff)?;
    if prior.as_ref().is_some_and(|raw| *raw != bytes) {
        return Err(Error::Conflict);
    }
    publish(dir, &record, &bytes, 8192)?;
    Ok(handoff)
}
pub async fn redeem(config: &Path) -> Result<Installation, Error> {
    redeem_kind(config, false).await
}
pub async fn redeem_rotation(config: &Path) -> Result<Installation, Error> {
    redeem_kind(config, true).await
}
async fn redeem_kind(config: &Path, require_rotation: bool) -> Result<Installation, Error> {
    let c: Config = serde_json::from_slice(&input(config, 8192)?).map_err(|_| Error::Config)?;
    if c.version != 1
        || c.environment != "development"
        || !uuid4(&c.request_id)
        || !orange_protocol::valid_digest(&c.issuer_sha256)
    {
        return Err(Error::Config);
    }
    let ca = verify::issuer(&input(&c.issuer_certificate, 32768)?, &c.issuer_sha256)?;
    let (http, pin) = client(&c)?;
    let dir = open_directory(&c.device_directory)?;
    lock(&dir)?;
    let id = &c.request_id;
    let key = spki(&dir, id)?;
    let intent_name = format!("{id}.redemption.json");
    let receipt_name = format!("{id}.receipt.json");
    let pem_name = format!("{id}.client.pem");
    let cached = read_at(&dir, receipt_name.as_ref(), 32768)?;
    let prior = read_at(&dir, intent_name.as_ref(), 4096)?;
    // Recover a confirmed local publication without retaining or requiring the one-time secret.
    let secret = if cached.is_some() {
        None
    } else {
        Some(parse_token(&input(&c.exchange_token_file, 46)?)?)
    };
    let intent: Intent = match prior {
        Some(raw) => decode(&raw)?,
        None if cached.is_none() => Intent {
            version: 1,
            request_id: id.clone(),
            redemption_id: Uuid::new_v4().to_string(),
            origin: c.api_origin.clone(),
            spki_sha256: key.clone(),
            issuer_sha256: c.issuer_sha256.clone(),
            token_sha256: hash(secret.as_ref().ok_or(Error::Config)?.as_bytes()),
            api_ca_sha256: pin.clone(),
        },
        None => return Err(Error::Conflict),
    };
    if intent.version != 1
        || !uuid4(&intent.redemption_id)
        || intent.request_id != *id
        || intent.origin != c.api_origin
        || intent.spki_sha256 != key
        || intent.issuer_sha256 != c.issuer_sha256
        || intent.api_ca_sha256 != pin
        || secret
            .as_ref()
            .is_some_and(|s| hash(s.as_bytes()) != intent.token_sha256)
    {
        return Err(Error::Conflict);
    }
    let rotation_node = rotation::bound_node(&dir, &intent, require_rotation)?;
    let intent_bytes = encode(&intent)?;
    publish(&dir, &intent_name, &intent_bytes, 4096)?;
    fs::flock(&dir, fs::FlockOperation::Unlock)?;
    let (receipt, source) = match cached {
        Some(bytes) => (decode(&bytes)?, "local_receipt"),
        None => (
            exchange(&c, http, &intent, secret.as_deref().ok_or(Error::Config)?).await?,
            "gateway_receipt",
        ),
    };
    lock(&dir)?;
    let named = fs::fstat(open_directory(&c.device_directory)?)?;
    let directory = fs::fstat(&dir)?;
    if directory.st_dev != named.st_dev || directory.st_ino != named.st_ino {
        return Err(Error::Local);
    }
    verify::leaf(&receipt, &intent, &ca)?;
    if rotation::bound_node(&dir, &intent, require_rotation)? != rotation_node
        || rotation_node
            .as_ref()
            .is_some_and(|node| node != &receipt.node_id)
    {
        return Err(Error::Certificate);
    }
    if spki(&dir, id)? != key
        || read_at(&dir, intent_name.as_ref(), 4096)?.as_deref() != Some(intent_bytes.as_slice())
    {
        return Err(Error::Conflict);
    }
    publish(&dir, &receipt_name, &encode(&receipt)?, 32768)?;
    publish(&dir, &pem_name, receipt.certificate_pem.as_bytes(), 16384)?;
    Ok(Installation {
        version: 1,
        request_id: id.clone(),
        redemption_id: intent.redemption_id,
        node_id: receipt.node_id,
        spki_sha256: key,
        certificate_sha256: receipt.certificate_sha256,
        certificate: c.device_directory.join(pem_name),
        private_key: c.device_directory.join(format!("{id}.key.pem")),
        expires_unix: receipt.expires_unix,
        source,
    })
}
