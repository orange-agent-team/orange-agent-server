use super::BrokerError;
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{self, File},
    path::Path,
};
pub(super) fn open(path: &Path) -> Result<(Connection, File), BrokerError> {
    let parent = path
        .parent()
        .ok_or(BrokerError::StoragePolicy)?
        .canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(&parent)?.mode() & 0o077 != 0 {
            return Err(BrokerError::StoragePolicy);
        }
    }
    let path = parent.join(path.file_name().ok_or(BrokerError::StoragePolicy)?);
    let name = path.to_str().ok_or(BrokerError::StoragePolicy)?;
    let lock = private_file(&path.with_extension("broker-lock"))?;
    lock.try_lock().map_err(|_| BrokerError::Busy)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = std::path::PathBuf::from(format!("{name}{suffix}"));
        if fs::symlink_metadata(&side).is_ok() {
            validate(&side)?;
        }
    }
    drop(private_file(&path)?);
    Ok((
        Connection::open_with_flags(path, OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW)?,
        lock,
    ))
}
fn validate(path: &Path) -> Result<(), BrokerError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(BrokerError::StoragePolicy);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(BrokerError::StoragePolicy);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(BrokerError::StoragePolicy);
        }
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<File, BrokerError> {
    if fs::symlink_metadata(path).is_ok() {
        validate(path)?;
    }
    let mut options = File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    validate(path)?;
    if !file.metadata()?.is_file() {
        return Err(BrokerError::StoragePolicy);
    }
    Ok(file)
}
