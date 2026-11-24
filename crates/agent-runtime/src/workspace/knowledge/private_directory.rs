//! Verify the OS boundary around a saved collection before opening SQLite or its sidecars.
use super::*;

#[cfg(unix)]
pub(super) fn check(path: &Path) -> Result<(), KnowledgeError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(WorkspaceError::Io)?;
    if meta.file_type().is_symlink()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(WorkspaceError::Forbidden.into());
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn check(path: &Path) -> Result<(), KnowledgeError> {
    use std::os::windows::fs::MetadataExt;
    use std::process::{Command, Stdio};
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or(KnowledgeError::Storage)?;
    // The path travels as data in the child environment, never as PowerShell source.
    let exe = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let meta = std::fs::symlink_metadata(path).map_err(WorkspaceError::Io)?;
    if meta.file_attributes() & 0x400 != 0 {
        return Err(WorkspaceError::Forbidden.into());
    }
    let mut child = Command::new(exe)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            include_str!("private_directory.ps1"),
        ])
        .env_clear()
        .env("SystemRoot", &root)
        .env("ORANGE_KNOWLEDGE_ACL_PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(WorkspaceError::Io)?;
    let end = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().map_err(WorkspaceError::Io)? {
            return if status.success() {
                Ok(())
            } else {
                Err(WorkspaceError::Forbidden.into())
            };
        }
        if std::time::Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WorkspaceError::Timeout.into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
