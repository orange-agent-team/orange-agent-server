#[cfg(unix)]
pub(crate) fn read_private(path: &std::path::Path) -> Result<Vec<u8>, &'static str> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "source config unreadable")?;
    let info = file.metadata().map_err(|_| "source config unreadable")?;
    if !info.is_file()
        || info.nlink() != 1
        || info.mode() & 0o077 != 0
        || info.uid() != rustix::process::geteuid().as_raw()
    {
        return Err("private owner source file required");
    }
    let mut bytes = Vec::new();
    file.take(131073)
        .read_to_end(&mut bytes)
        .map_err(|_| "source config unreadable")?;
    if bytes.len() > 131072 {
        return Err("source config too large");
    }
    Ok(bytes)
}
