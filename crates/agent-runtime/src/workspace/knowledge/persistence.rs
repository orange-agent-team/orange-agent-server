//! Private single-writer collection storage. Raw text, queries and file names stay in memory.
use super::*;
use crate::storage::{HostLock, open_private};
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{OpenFlags, TransactionBehavior};

const APP_ID: i64 = 0x4f414b31;
const AUTHORITY_ID: i64 = 0x4f414b41;
const SCHEMA: &str = "
CREATE TABLE docs(path TEXT PRIMARY KEY,sha TEXT NOT NULL,stamp TEXT NOT NULL, UNIQUE(path,sha));
CREATE INDEX docs_by_hash ON docs(sha,path);
CREATE VIRTUAL TABLE chunks USING fts5(terms,sha UNINDEXED,lo UNINDEXED,hi UNINDEXED,tokenize='unicode61 remove_diacritics 0');
INSERT INTO chunks(chunks,rank) VALUES('secure-delete',1);
CREATE TABLE issued(hit TEXT PRIMARY KEY,path TEXT NOT NULL,sha TEXT NOT NULL,lo INTEGER NOT NULL,
 hi INTEGER NOT NULL, UNIQUE(path,sha,lo,hi), FOREIGN KEY(path,sha) REFERENCES docs(path,sha),CHECK(lo>=0 AND hi>lo AND hi-lo<=1024));";

pub(super) struct PrivateIndex {
    _owner: HostLock,
    mac: Hmac<Sha256>,
    authority: Authority,
}
impl PrivateIndex {
    pub(super) fn tag(&self, namespace: &[u8], text: &str) -> String {
        let mut mac = self.mac.clone();
        mac.update(namespace);
        mac.update(text.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}
fn namespace(owner: &str, id: &str) -> Result<String, KnowledgeError> {
    request_digest("local-knowledge-store-name.v1", &(owner, id))
        .map_err(|_| KnowledgeError::Invalid)
}
fn file(directory: &Path, owner: &str, id: &str) -> Result<PathBuf, KnowledgeError> {
    if !valid_id(owner) || !valid_id(id) {
        return Err(KnowledgeError::Invalid);
    }
    let name = namespace(owner, id)?;
    Ok(directory.join(format!(".orange-agent-tmp-knowledge-{name}.sqlite")))
}
fn connect(path: &Path) -> Result<(Connection, HostLock), KnowledgeError> {
    let parent = path.parent().ok_or(KnowledgeError::Invalid)?;
    super::private_directory::check(parent)?;
    for member in [path.to_path_buf(), path.with_extension("host-lock")]
        .into_iter()
        .chain(["-wal", "-shm", "-journal"].map(|suffix| {
            let mut file = path.as_os_str().to_os_string();
            file.push(suffix);
            PathBuf::from(file)
        }))
    {
        match std::fs::symlink_metadata(&member) {
            Ok(_) => super::private_directory::check(&member)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(WorkspaceError::Io(e).into()),
        }
    }
    let (normalized, lock) = open_private(path).map_err(|e| match e {
        crate::JournalError::Busy => KnowledgeError::Busy,
        _ => KnowledgeError::Storage,
    })?;
    super::private_directory::check(&normalized)?;
    super::private_directory::check(&normalized.with_extension("host-lock"))?;
    let db = Connection::open_with_flags(
        normalized,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA secure_delete=ON;",
    )?;
    Ok((db, lock))
}
fn format(db: &Connection) -> Result<(i64, i64, i64), KnowledgeError> {
    Ok((
        db.pragma_query_value(None, "application_id", |r| r.get(0))?,
        db.pragma_query_value(None, "user_version", |r| r.get(0))?,
        db.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?,
    ))
}
struct Authority {
    db: Connection,
    _lock: HostLock,
}
impl Authority {
    fn open(
        directory: &Path,
        ns: &str,
        scope: Option<&str>,
        create: bool,
    ) -> Result<Self, KnowledgeError> {
        let path = directory.join(format!(".orange-agent-tmp-knowledge-authority-{ns}.sqlite"));
        if !create && !path.try_exists().map_err(WorkspaceError::Io)? {
            return Err(KnowledgeError::Storage);
        }
        let (mut db, lock) = connect(&path)?;
        let info = format(&db)?;
        if info != (0, 0, 0) && (info.0 != AUTHORITY_ID || info.1 != 1) {
            return Err(KnowledgeError::Storage);
        }
        db.execute_batch(
            "PRAGMA synchronous=EXTRA;PRAGMA fullfsync=ON;PRAGMA journal_mode=DELETE;",
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if info == (0, 0, 0) {
            if !create {
                return Err(KnowledgeError::Storage);
            }
            // A registered selection may be withdrawn before initialization finished.
            let binding = scope.unwrap_or("retired-before-initialization");
            tx.execute_batch("CREATE TABLE authority(singleton INTEGER PRIMARY KEY CHECK(singleton=1),namespace TEXT NOT NULL,scope TEXT NOT NULL,secret BLOB,
                 CHECK(secret IS NULL OR (typeof(secret)='blob' AND length(secret)=32)));
                CREATE TRIGGER immutable_retirement BEFORE UPDATE ON authority
                WHEN OLD.secret IS NULL OR NEW.secret IS NOT NULL OR NEW.namespace!=OLD.namespace OR NEW.scope!=OLD.scope
                BEGIN SELECT RAISE(ABORT,'invalid authority transition');END;")?;
            tx.execute(
                "INSERT INTO authority VALUES(1,?,?,CASE WHEN ? THEN randomblob(32) ELSE NULL END)",
                params![ns, binding, scope.is_some()],
            )?;
            tx.pragma_update(None, "application_id", AUTHORITY_ID)?;
            tx.pragma_update(None, "user_version", 1)?;
        }
        let (stored, stored_scope): (String, String) = tx.query_row(
            "SELECT namespace,scope FROM authority WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if stored != ns || scope.is_some_and(|s| s != stored_scope) {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        tx.commit()?;
        Ok(Self { db, _lock: lock })
    }
    fn key(&self) -> Result<Option<Vec<u8>>, KnowledgeError> {
        Ok(self
            .db
            .query_row("SELECT secret FROM authority WHERE singleton=1", [], |r| {
                r.get(0)
            })?)
    }
    fn revoke(&mut self) -> Result<(), KnowledgeError> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE authority SET secret=NULL WHERE singleton=1 AND secret IS NOT NULL",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn protected_directory(
    grant: &WorkspaceGrant,
    directory: &Path,
) -> Result<PathBuf, KnowledgeError> {
    let protected = directory.canonicalize().map_err(WorkspaceError::Io)?;
    let opened = Dir::open_ambient_dir(&protected, cap_std::ambient_authority())
        .map_err(WorkspaceError::Io)?;
    let expected = directory_identity(&opened)?;
    if grant.inner.private_directories.get(&protected) != Some(&expected)
        || ancestor_identity_matches(&protected, directory_identity(&grant.inner.directory)?)?
    {
        return Err(WorkspaceError::Forbidden.into());
    }
    Ok(protected)
}
pub(super) fn open(
    directory: &Path,
    authority_root: &Path,
    grant: &WorkspaceGrant,
    id: &str,
    paths: &BTreeSet<String>,
) -> Result<Index, KnowledgeError> {
    if !grant.has_persistent_identity() {
        return Err(WorkspaceError::PersistentIdentityUnavailable.into());
    }
    let protected = protected_directory(grant, directory)?;
    let authority_dir = protected_directory(grant, authority_root)?;
    let ns = namespace(grant.owner(), id)?;
    let binding = request_digest(
        "local-knowledge-store-selection.v1",
        &(
            grant.owner(),
            grant.environment(),
            grant.resource_identity(),
            grant.policy(),
            id,
            paths,
        ),
    )
    .map_err(|_| KnowledgeError::Invalid)?;
    let (mut db, lock) = connect(&file(&protected, grant.owner(), id)?)?;
    let info = format(&db)?;
    if info != (0, 0, 0) && (info.0 != APP_ID || info.1 != 1) {
        return Err(KnowledgeError::Storage);
    }
    let authority = Authority::open(&authority_dir, &ns, Some(&binding), info == (0, 0, 0))?;
    let secret = match authority.key()? {
        Some(key) => key,
        None => {
            if info != (0, 0, 0) {
                purge_if_matching(&mut db, &ns)?;
            }
            return Err(WorkspaceError::Revoked.into());
        }
    };
    let verifier = hex::encode(Sha256::digest(&secret));
    db.execute_batch("PRAGMA journal_mode=DELETE;PRAGMA synchronous=EXTRA;PRAGMA fullfsync=ON;PRAGMA max_page_count=65536;")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if info == (0, 0, 0) {
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch("CREATE TABLE binding(singleton INTEGER PRIMARY KEY CHECK(singleton=1),scope TEXT NOT NULL,namespace TEXT NOT NULL,verifier TEXT NOT NULL CHECK(length(verifier)=64),active INTEGER NOT NULL CHECK(active IN (0,1)));
            CREATE TRIGGER irreversible_withdrawal BEFORE UPDATE ON binding WHEN OLD.active=0 OR NEW.active!=0 OR NEW.scope!=OLD.scope OR NEW.namespace!=OLD.namespace OR NEW.verifier!=OLD.verifier
            BEGIN SELECT RAISE(ABORT,'invalid collection transition');END;")?;
        tx.execute(
            "INSERT INTO binding VALUES(1,?,?,?,1)",
            params![binding, ns, verifier],
        )?;
        tx.pragma_update(None, "application_id", APP_ID)?;
        tx.pragma_update(None, "user_version", 1)?;
    }
    let (scope, active, expected): (String, bool, String) = tx.query_row(
        "SELECT scope,active,verifier FROM binding WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if !active {
        return Err(WorkspaceError::Revoked.into());
    }
    if scope != binding || expected != verifier {
        return Err(WorkspaceError::EnvironmentChanged.into());
    }
    let mac = Hmac::<Sha256>::new_from_slice(&secret).map_err(|_| KnowledgeError::Storage)?;
    tx.commit()?;
    let private = PrivateIndex {
        _owner: lock,
        mac,
        authority,
    };
    let permitted: BTreeSet<_> = paths.iter().map(|p| private.tag(b"path\0", p)).collect();
    {
        let mut query =
            db.prepare("SELECT path FROM docs UNION SELECT path FROM issued LIMIT 33")?;
        let stored = query.query_map([], |r| r.get::<_, String>(0))?;
        for entry in stored {
            if !permitted.contains(&entry?) {
                return Err(KnowledgeError::Storage);
            }
        }
    }
    Index::initialized(db, Some(private))
}
pub(super) fn transient() -> Result<Index, KnowledgeError> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY")?;
    db.execute_batch(SCHEMA)?;
    Index::initialized(db, None)
}
fn purge(db: &mut Connection) -> Result<(), KnowledgeError> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE binding SET active=0 WHERE singleton=1 AND active=1",
        [],
    )?;
    tx.execute("DELETE FROM issued", [])?;
    tx.execute("DELETE FROM docs", [])?;
    tx.execute("DELETE FROM chunks", [])?;
    tx.commit()?;
    Ok(())
}
fn purge_if_matching(db: &mut Connection, ns: &str) -> Result<(), KnowledgeError> {
    let (app, version, _) = format(db)?;
    if app != APP_ID || version != 1 {
        return Err(KnowledgeError::Storage);
    }
    let registered: String =
        db.query_row("SELECT namespace FROM binding WHERE singleton=1", [], |r| {
            r.get(0)
        })?;
    if registered != ns {
        return Err(WorkspaceError::EnvironmentChanged.into());
    }
    purge(db)
}
pub(super) fn withdraw(index: &mut Index) -> Result<(), KnowledgeError> {
    let private = index.private.as_mut().ok_or(KnowledgeError::Invalid)?;
    private.authority.revoke()?;
    purge(&mut index.db)
}
/// Withdraw a suspended store, including when the original source directory has disappeared.
pub fn withdraw_closed(directory: &Path, owner: &str, id: &str) -> Result<(), KnowledgeError> {
    withdraw_closed_with_authority(directory, directory, owner, id)
}
pub fn withdraw_closed_with_authority(
    directory: &Path,
    authority_dir: &Path,
    owner: &str,
    id: &str,
) -> Result<(), KnowledgeError> {
    retire_closed(directory, authority_dir, owner, id, false)
}
pub(super) fn retire_closed(
    directory: &Path,
    authority_dir: &Path,
    owner: &str,
    id: &str,
    registered: bool,
) -> Result<(), KnowledgeError> {
    let ns = namespace(owner, id)?;
    let path = file(directory, owner, id)?;
    // Lock order matches open(). The authoritative tombstone outlives the cached index.
    if path.try_exists().map_err(WorkspaceError::Io)? {
        let (mut db, _owner) = connect(&path)?;
        let mut authority = Authority::open(authority_dir, &ns, None, registered)?;
        authority.revoke()?;
        if registered && format(&db)? == (0, 0, 0) {
            return Ok(());
        }
        purge_if_matching(&mut db, &ns)
    } else {
        let mut authority = Authority::open(authority_dir, &ns, None, registered)?;
        authority.revoke()
    }
}
