//! Keep one device key in a caller-owned directory. Only public PKCS#10 evidence leaves this module.
pub mod redemption;
use orange_protocol::device_enrollment::WorkerCertificateRequest;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, PKCS_ED25519, PublicKeyData,
};
use rustix::fs::{self, FlockOperation, Mode, OFlags, RenameFlags};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("expected an absolute owner-only directory and a canonical v4 request UUID")]
    Invalid,
    #[error("device key is unavailable or has unsafe file permissions")]
    PrivateKey,
    #[error("another enrollment writer is busy")]
    Busy,
    #[error("device key or certificate request format is invalid")]
    Format,
    #[error("device enrollment storage operation failed")]
    Storage(#[from] io::Error),
}
impl From<rustix::io::Errno> for Error {
    fn from(err: rustix::io::Errno) -> Self {
        Self::Storage(err.into())
    }
}

fn checked_file(file: &File) -> Result<(), Error> {
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
        || m.len() > 4096
    {
        return Err(Error::PrivateKey);
    }
    Ok(())
}
fn read_key(file: &File) -> Result<KeyPair, Error> {
    checked_file(file)?;
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(Error::PrivateKey);
    }
    let pem = std::str::from_utf8(&bytes).map_err(|_| Error::Format)?;
    let key = KeyPair::from_pem(pem).map_err(|_| Error::Format)?;
    if key.algorithm() != &PKCS_ED25519 || key.serialize_pem().as_bytes() != bytes {
        return Err(Error::Format);
    }
    Ok(key)
}
fn lock(root: &File) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match fs::flock(root, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(Error::Busy),
            Err(e) => return Err(e.into()),
        }
    }
}
/// OpenSSL 3.0 expects RFC 5208 without the optional RFC 5958 public-key field.
/// Keep the provider-generated seed and independently compare the resulting public identity.
fn portable(key: &KeyPair) -> Result<Option<KeyPair>, Error> {
    use pkcs8::{
        PrivateKeyInfoRef,
        der::{Decode, Encode},
    };
    let encoded = key.serialize_der();
    let mut info = PrivateKeyInfoRef::from_der(&encoded).map_err(|_| Error::Format)?;
    if info.public_key.take().is_none() {
        return Ok(None);
    }
    let normalized =
        KeyPair::try_from(info.to_der().map_err(|_| Error::Format)?).map_err(|_| Error::Format)?;
    if normalized.algorithm() != &PKCS_ED25519
        || normalized.subject_public_key_info() != key.subject_public_key_info()
    {
        return Err(Error::Format);
    }
    Ok(Some(normalized))
}
fn durable_key(root: &File, id: &str, create: bool) -> Result<KeyPair, Error> {
    let name = format!("{id}.key.pem");
    let flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let existing = match fs::openat(root, name.as_str(), OFlags::RDONLY | flags, Mode::empty()) {
        Ok(fd) => {
            let original = File::from(fd);
            let key = read_key(&original)?;
            let Some(normalized) = portable(&key)? else {
                root.sync_all()?;
                return Ok(key);
            };
            Some((original, normalized))
        }
        Err(rustix::io::Errno::NOENT) if create => None,
        Err(e) => return Err(e.into()),
    };
    let pending = format!(".{id}.key.pending");
    let mut stage = File::from(fs::openat(
        root,
        pending.as_str(),
        OFlags::RDWR | OFlags::CREATE | flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    checked_file(&stage)?;
    let (key, legacy) = match existing {
        Some((old, key)) => (key, Some(old)),
        None => {
            let key = match read_key(&stage) {
                Ok(key) => key,
                Err(Error::Format) => {
                    KeyPair::generate_for(&PKCS_ED25519).map_err(|_| Error::Format)?
                }
                Err(e) => return Err(e),
            };
            (portable(&key)?.unwrap_or(key), None)
        }
    };
    stage.rewind()?;
    stage.set_len(0)?;
    stage.write_all(key.serialize_pem().as_bytes())?;
    stage.sync_all()?;
    if let Some(old) = legacy {
        checked_file(&old)?;
        let prior = fs::fstat(&old)?;
        let named = fs::statat(root, name.as_str(), fs::AtFlags::SYMLINK_NOFOLLOW)?;
        if named.st_dev != prior.st_dev || named.st_ino != prior.st_ino {
            return Err(Error::PrivateKey);
        }
        fs::renameat(root, pending.as_str(), root, name.as_str())?;
    } else {
        fs::renameat_with(
            root,
            pending.as_str(),
            root,
            name.as_str(),
            RenameFlags::NOREPLACE,
        )?;
    }
    root.sync_all()?;
    Ok(key)
}
fn open_directory(root: &Path) -> Result<File, Error> {
    if !root.is_absolute() {
        return Err(Error::Invalid);
    }
    let directory = File::from(fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let m = directory.metadata()?;
    if m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
        return Err(Error::Invalid);
    }
    Ok(directory)
}
fn validate_id(id: &str) -> Result<(), Error> {
    let request = Uuid::parse_str(id).map_err(|_| Error::Invalid)?;
    if request.get_variant() != uuid::Variant::RFC4122
        || request.get_version_num() != 4
        || request.to_string() != id
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
pub fn prepare(root: &Path, id: &str) -> Result<WorkerCertificateRequest, Error> {
    validate_id(id)?;
    let directory = open_directory(root)?;
    lock(&directory)?;
    request(&durable_key(&directory, id, true)?, id)
}
fn request(key: &KeyPair, id: &str) -> Result<WorkerCertificateRequest, Error> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|_| Error::Format)?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("orange-worker-{id}"));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let csr = params.serialize_request(key).map_err(|_| Error::Format)?;
    Ok(WorkerCertificateRequest {
        version: 1,
        request_id: id.into(),
        purpose: "public_worker".into(),
        spki_sha256: hex::encode(Sha256::digest(key.subject_public_key_info())),
        csr_sha256: hex::encode(Sha256::digest(csr.der().as_ref())),
        csr_pem: csr.pem().map_err(|_| Error::Format)?,
    })
}
