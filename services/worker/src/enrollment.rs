//! Keep one device key in a caller-owned directory. Only public PKCS#10 evidence leaves this module.
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
fn durable_key(root: &File, id: &str) -> Result<KeyPair, Error> {
    let name = format!("{id}.key.pem");
    let flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    match fs::openat(root, name.as_str(), OFlags::RDONLY | flags, Mode::empty()) {
        Ok(fd) => {
            let key = read_key(&File::from(fd))?;
            root.sync_all()?;
            return Ok(key);
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let pending = format!(".{id}.key.pending");
    // The reserved staging file never has a visible CSR. An interrupted write can be replaced
    // under the directory lock, without opening or changing a published key.
    let mut file = File::from(fs::openat(
        root,
        pending.as_str(),
        OFlags::RDWR | OFlags::CREATE | flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    checked_file(&file)?;
    let key = match read_key(&file) {
        Ok(key) => key,
        Err(Error::Format) => {
            let key = KeyPair::generate_for(&PKCS_ED25519).map_err(|_| Error::Format)?;
            file.rewind()?;
            file.set_len(0)?;
            file.write_all(key.serialize_pem().as_bytes())?;
            key
        }
        Err(e) => return Err(e),
    };
    file.sync_all()?;
    fs::renameat_with(
        root,
        pending.as_str(),
        root,
        name.as_str(),
        RenameFlags::NOREPLACE,
    )?;
    root.sync_all()?;
    Ok(key)
}
pub fn prepare(root: &Path, id: &str) -> Result<WorkerCertificateRequest, Error> {
    let request = Uuid::parse_str(id).map_err(|_| Error::Invalid)?;
    if !root.is_absolute()
        || request.get_variant() != uuid::Variant::RFC4122
        || request.get_version_num() != 4
        || request.to_string() != id
    {
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
    lock(&directory)?;
    let key = durable_key(&directory, id)?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|_| Error::Format)?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("orange-worker-{id}"));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let csr = params.serialize_request(&key).map_err(|_| Error::Format)?;
    Ok(WorkerCertificateRequest {
        version: 1,
        request_id: id.into(),
        purpose: "public_worker".into(),
        spki_sha256: hex::encode(Sha256::digest(key.subject_public_key_info())),
        csr_sha256: hex::encode(Sha256::digest(csr.der().as_ref())),
        csr_pem: csr.pem().map_err(|_| Error::Format)?,
    })
}
