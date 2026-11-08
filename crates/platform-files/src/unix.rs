use sha2::{Digest, Sha256};
use std::{io, os::fd::AsFd};
const MAX_NAMES: usize = 16 * 1024;
const MAX_VALUE: usize = 64 * 1024;
const MAX_TOTAL: usize = 256 * 1024;
#[derive(Clone, PartialEq, Eq)]
pub struct SecurityMetadata {
    uid: u32,
    gid: u32,
    mode: u32,
    flags: u32,
    attributes: Vec<(Vec<u8>, Vec<u8>)>,
    acl: Vec<u8>,
}
impl std::fmt::Debug for SecurityMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityMetadata")
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("mode", &self.mode)
            .field("attribute_count", &self.attributes.len())
            .field("acl_bytes", &self.acl.len())
            .finish_non_exhaustive()
    }
}
impl SecurityMetadata {
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"orange-security-metadata.v1");
        for value in [self.uid, self.gid, self.mode, self.flags] {
            hash.update(value.to_be_bytes());
        }
        hash.update((self.attributes.len() as u64).to_be_bytes());
        for (name, value) in &self.attributes {
            hash.update((name.len() as u64).to_be_bytes());
            hash.update(name);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
        hash.update((self.acl.len() as u64).to_be_bytes());
        hash.update(&self.acl);
        hash.finalize().into()
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub fn security_metadata(file: &impl AsFd) -> io::Result<SecurityMetadata> {
    let stat = rustix::fs::fstat(file)?;
    let mut names = vec![0u8; MAX_NAMES];
    let count = rustix::fs::flistxattr(file, names.as_mut_slice())?;
    names.truncate(count);
    if !names.is_empty() && !names.ends_with(&[0]) {
        return Err(invalid("invalid attribute list"));
    }
    let mut attributes = Vec::new();
    let mut total = 0usize;
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if attributes.len() >= 128 {
            return Err(invalid("attribute count limit"));
        }
        let mut value = vec![0u8; MAX_VALUE];
        let count = rustix::fs::fgetxattr(file, name, value.as_mut_slice())?;
        value.truncate(count);
        total = total
            .checked_add(name.len() + value.len())
            .ok_or_else(|| invalid("metadata size overflow"))?;
        if total > MAX_TOTAL {
            return Err(invalid("metadata size limit"));
        }
        attributes.push((name.to_vec(), value));
    }
    attributes.sort_by(|a, b| a.0.cmp(&b.0));
    #[cfg(target_vendor = "apple")]
    let (flags, acl) = (stat.st_flags, crate::apple::acl_bytes(file)?);
    #[cfg(not(target_vendor = "apple"))]
    let (flags, acl) = (0, Vec::new());
    // Metadata reads span several syscalls. Refuse observable concurrent
    // changes instead of approving a mixture of old and new properties.
    let after = rustix::fs::fstat(file)?;
    if stat.st_uid != after.st_uid
        || stat.st_gid != after.st_gid
        || stat.st_mode != after.st_mode
        || stat.st_nlink != after.st_nlink
        || stat.st_size != after.st_size
        || stat.st_ctime != after.st_ctime
        || stat.st_ctime_nsec != after.st_ctime_nsec
        || stat.st_mtime != after.st_mtime
        || stat.st_mtime_nsec != after.st_mtime_nsec
    {
        return Err(invalid("file changed while reading security metadata"));
    }
    #[cfg(target_vendor = "apple")]
    if flags != after.st_flags {
        return Err(invalid(
            "file flags changed while reading security metadata",
        ));
    }
    Ok(SecurityMetadata {
        uid: stat.st_uid,
        gid: stat.st_gid,
        mode: u32::from(stat.st_mode) & 0o7777,
        flags,
        attributes,
        acl,
    })
}
/// Copy only metadata to a distinct, already-open staging file. Contents are
/// untouched. Caller must recheck after writing data and before publication.
pub fn copy_security_metadata(
    source: &impl AsFd,
    target: &impl AsFd,
) -> io::Result<SecurityMetadata> {
    let a = rustix::fs::fstat(source)?;
    let b = rustix::fs::fstat(target)?;
    if a.st_dev == b.st_dev && a.st_ino == b.st_ino {
        return Err(invalid("source and staging file are identical"));
    }
    if rustix::fs::FileType::from_raw_mode(a.st_mode) != rustix::fs::FileType::RegularFile
        || rustix::fs::FileType::from_raw_mode(b.st_mode) != rustix::fs::FileType::RegularFile
        || a.st_nlink != 1
        || b.st_nlink != 1
    {
        return Err(invalid(
            "metadata copy requires distinct single-link regular files",
        ));
    }
    let expected = security_metadata(source)?;
    #[cfg(target_vendor = "apple")]
    crate::apple::copy_security(source, target)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::process::{Gid, Uid};
        rustix::fs::fchown(
            target,
            Some(Uid::from_raw(expected.uid)),
            Some(Gid::from_raw(expected.gid)),
        )?;
        let current = security_metadata(target)?;
        for (name, _) in current.attributes {
            if !expected.attributes.iter().any(|(n, _)| *n == name) {
                rustix::fs::fremovexattr(target, name.as_slice())?;
            }
        }
        for (name, value) in &expected.attributes {
            rustix::fs::fsetxattr(
                target,
                name.as_slice(),
                value,
                rustix::fs::XattrFlags::empty(),
            )?;
        }
        rustix::fs::fchmod(target, rustix::fs::Mode::from_raw_mode(expected.mode))?;
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "metadata backend unavailable",
    ));
    if security_metadata(source)? != expected || security_metadata(target)? != expected {
        return Err(invalid("security metadata changed or was not preserved"));
    }
    Ok(expected)
}
#[cfg(test)]
mod tests;
