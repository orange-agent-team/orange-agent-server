use crate::JournalError;
use rusqlite::Connection;
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

/// One owned open-file description, retained only by explicit effect lifetimes.
/// No duplicated descriptor can release the lock while an authorized worker lives.
pub(crate) struct HostLock {
    inner: std::sync::Arc<OwnedHostLock>,
}
struct OwnedHostLock {
    file: File,
}
pub(crate) struct HostEffectLease {
    _inner: std::sync::Arc<OwnedHostLock>,
}
impl HostLock {
    pub(crate) fn effect_lease(&self) -> HostEffectLease {
        HostEffectLease {
            _inner: self.inner.clone(),
        }
    }
}
impl Drop for OwnedHostLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A private trusted host directory is required; never silently chmod caller data.
pub(crate) fn open_private(path: &Path) -> Result<(PathBuf, HostLock), JournalError> {
    let parent = path.parent().ok_or(JournalError::Invalid)?.canonicalize()?;
    let metadata = fs::metadata(&parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(JournalError::Invalid);
        }
    }
    if !metadata.is_dir() {
        return Err(JournalError::Invalid);
    }
    let normalized = parent.join(path.file_name().ok_or(JournalError::Invalid)?);
    normalized.to_str().ok_or(JournalError::Invalid)?;
    let lock = private_file(&normalized.with_extension("host-lock"))?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => JournalError::Busy,
        std::fs::TryLockError::Error(error) => JournalError::Io(error),
    })?;
    let lock = HostLock {
        inner: std::sync::Arc::new(OwnedHostLock { file: lock }),
    };
    // Sidecars must also be regular private files, never links into another location.
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", normalized.to_string_lossy()));
        if fs::symlink_metadata(&sidecar).is_ok() {
            validate_file(&sidecar)?;
        }
    }
    drop(private_file(&normalized)?);
    Ok((normalized, lock))
}
fn validate_file(path: &Path) -> Result<(), JournalError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(JournalError::Invalid);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(JournalError::Invalid);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(JournalError::Invalid);
        }
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<File, JournalError> {
    if fs::symlink_metadata(path).is_ok() {
        validate_file(path)?;
    }
    let mut options = OpenOptions::new();
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
    validate_file(path)?;
    if !file.metadata()?.is_file() {
        return Err(JournalError::Invalid);
    }
    Ok(file)
}
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), JournalError> {
    connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let result = (|| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("schema_v2.sql"))?;
        let broken: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if broken != 0 {
            return Err(JournalError::Conflict);
        }
        tx.commit()?;
        Ok(())
    })();
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    result
}
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), JournalError> {
    connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let result = (|| {
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("schema_v4.sql"))?;
        let broken: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if broken != 0 {
            return Err(JournalError::Conflict);
        }
        tx.commit()?;
        Ok(())
    })();
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    result
}

#[cfg(test)]
pub(crate) fn private_temp() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("private");
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Journal;
    #[test]
    fn graceful_journal_close_releases_its_lock_despite_a_duplicate_descriptor() {
        let dir = private_temp();
        let path = dir.path().join("journal.sqlite");
        let first = Journal::open(&path).expect("first host");
        let duplicate = first
            ._host_lock
            .inner
            .file
            .try_clone()
            .expect("duplicate open-file description fixture");
        assert!(matches!(Journal::open(&path), Err(JournalError::Busy)));
        drop(first);
        let second = Journal::open(&path).expect("closed Journal no longer owns the host lock");
        // A stale duplicate must not release another host's distinct lock.
        // Releasing an already-unlocked descriptor may succeed or report an
        // error depending on the OS; neither may unlock the next owner.
        let _ = duplicate.unlock();
        drop(duplicate);
        assert!(matches!(Journal::open(&path), Err(JournalError::Busy)));
        drop(second);
        Journal::open(&path).expect("subsequent owner");
    }
    #[test]
    fn v1_migration_preserves_approval_and_unknown_and_foreign_keys() {
        let dir = private_temp();
        let path = dir.path().join("old.sqlite");
        drop(private_file(&path).expect("file"));
        let connection = Connection::open(&path).expect("db");
        connection.execute_batch("
            CREATE TABLE runs(run_id TEXT PRIMARY KEY,owner TEXT NOT NULL,policy TEXT NOT NULL,active INTEGER NOT NULL);
            CREATE TABLE responses(invocation TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES runs(run_id),attempt INTEGER NOT NULL,digest TEXT NOT NULL);
            CREATE TABLE operations(operation_id TEXT PRIMARY KEY,owner TEXT NOT NULL,run_id TEXT NOT NULL REFERENCES runs(run_id),invocation TEXT NOT NULL REFERENCES responses(invocation),argument_digest TEXT NOT NULL,tool_version TEXT NOT NULL,environment TEXT NOT NULL,policy TEXT NOT NULL,requires_approval INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN('intended','dispatching','succeeded','unknown')),result_digest TEXT);
            CREATE TABLE approvals(operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),actor TEXT NOT NULL,argument_digest TEXT NOT NULL,environment TEXT NOT NULL,policy TEXT NOT NULL,expires_at INTEGER NOT NULL);
            CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,operation_id TEXT,kind TEXT NOT NULL);
            INSERT INTO runs VALUES('run','alice','policy',1);
            INSERT INTO responses VALUES('inv','run',1,'old-response-digest');
            INSERT INTO operations VALUES('op','alice','run','inv','old-arguments','read.v1','env','policy',1,'dispatching',NULL);
            INSERT INTO approvals VALUES('op','alice','old-arguments','env','policy',99);
            PRAGMA user_version=1;
        ").expect("legacy fixture");
        drop(connection);
        let journal = Journal::open(&path).expect("migrate");
        let version: i64 = journal
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, 28);
        assert_eq!(
            journal.state("alice", "op").expect("state").as_deref(),
            Some("unknown")
        );
        let actor: String = journal
            .connection
            .query_row("SELECT actor FROM approvals", [], |r| r.get(0))
            .expect("approval");
        assert_eq!(actor, "alice");
        let body: Option<String> = journal
            .connection
            .query_row("SELECT body FROM responses", [], |r| r.get(0))
            .expect("body");
        assert!(body.is_none(), "never invent legacy response bodies");
        let broken: i64 = journal
            .connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .expect("fk");
        assert_eq!(broken, 0);
        assert!(
            journal
                .connection
                .execute("DELETE FROM operations WHERE operation_id='op'", [])
                .is_err(),
            "approval FK survives table rebuild"
        );
    }
    #[test]
    fn future_and_unknown_schemas_are_not_rewritten() {
        for version in [0, 26] {
            let dir = private_temp();
            let path = dir.path().join("future.sqlite");
            drop(private_file(&path).expect("file"));
            let connection = Connection::open(&path).expect("db");
            connection.execute_batch(&format!("CREATE TABLE future_data(value TEXT); INSERT INTO future_data VALUES('keep'); PRAGMA user_version={version};")).expect("schema");
            drop(connection);
            let before = fs::read(&path).expect("bytes");
            assert!(matches!(
                Journal::open(&path),
                Err(JournalError::UnsupportedVersion)
            ));
            assert_eq!(fs::read(path).expect("bytes"), before);
        }
    }
    #[cfg(unix)]
    #[test]
    fn nonprivate_directories_links_and_sidecar_links_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let private = private_temp();
        let other = private_temp();
        let target = other.path().join("secret");
        fs::write(&target, "keep").expect("file");
        let path = private.path().join("journal.sqlite");
        symlink(&target, &path).expect("link");
        assert!(Journal::open(&path).is_err());
        fs::remove_file(&path).expect("unlink");
        fs::hard_link(&target, &path).expect("hardlink");
        assert!(Journal::open(&path).is_err());
        fs::remove_file(&path).expect("unlink");
        let side = private.path().join("journal.sqlite-wal");
        symlink(&target, &side).expect("sidecar");
        assert!(Journal::open(&path).is_err());
        fs::remove_file(side).expect("unlink");
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(Journal::open(&path).is_err());
        assert_eq!(fs::read_to_string(target).expect("unchanged"), "keep");
    }
}
