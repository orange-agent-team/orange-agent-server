//! Selected-file retrieval. Persistence is an explicit host choice, with live revalidation after reopening.
mod persistence;
mod private_directory;
mod saved_collections;
use super::*;
use cap_fs_ext::MetadataExt;
pub use persistence::{withdraw_closed, withdraw_closed_with_authority};
use rusqlite::{Connection, params};
pub use saved_collections::SavedKnowledgeError;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use uuid::Uuid;

const MAX_DOCS: usize = 32;
const MAX_SOURCE: usize = 256 * 1024;
const CHUNK_BYTES: usize = 1024;
const MAX_HITS: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("invalid knowledge request")]
    Invalid,
    #[error("citation no longer available")]
    Stale,
    #[error("local text index is in use")]
    Busy,
    #[error("local text index unavailable")]
    Storage,
}
impl From<rusqlite::Error> for KnowledgeError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}
pub use orange_protocol::knowledge::{Hit, KnowledgeRequest, KnowledgeResult, Read, Search};

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check_live(collection: &KnowledgeCollection, stop: &AtomicBool) -> Result<(), KnowledgeError> {
    if collection.revoked.load(Ordering::Acquire) || collection.grant.is_revoked() {
        return Err(WorkspaceError::Revoked.into());
    }
    if stop.load(Ordering::Acquire) {
        return Err(WorkspaceError::Interrupted.into());
    }
    Ok(())
}

struct Source {
    _file: cap_std::fs::File,
    identity: (u64, u64, i128),
    sha: String,
}
struct Index {
    db: Connection,
    sources: BTreeMap<String, Source>,
    private: Option<persistence::PrivateIndex>,
}
#[derive(Clone)]
pub struct KnowledgeCollection {
    id: String,
    paths: Arc<BTreeSet<String>>,
    grant: WorkspaceGrant,
    index: Arc<Mutex<Index>>,
    revoked: Arc<AtomicBool>,
    publication: Arc<RwLock<()>>,
    journal_lease: Option<Arc<crate::storage::HostEffectLease>>,
}
fn selected_paths(paths: &[String]) -> Result<BTreeSet<&String>, KnowledgeError> {
    if paths.is_empty() || paths.len() > MAX_DOCS {
        return Err(KnowledgeError::Invalid);
    }
    let mut allowed = BTreeSet::new();
    for path in paths {
        components(path, false)?;
        if !allowed.insert(path) {
            return Err(KnowledgeError::Invalid);
        }
    }
    Ok(allowed)
}

impl KnowledgeCollection {
    pub fn collection_id(&self) -> &str {
        &self.id
    }
    /// Only the trusted host chooses files. A new grant/selection starts a new citation namespace.
    pub fn open(
        grant: WorkspaceGrant,
        id: &str,
        paths: Vec<String>,
    ) -> Result<Self, KnowledgeError> {
        Self::open_mode(grant, id, paths, None)
    }
    /// The caller must select the same files under a fresh live workspace grant after every restart.
    pub fn open_persistent(
        grant: WorkspaceGrant,
        id: &str,
        paths: Vec<String>,
        private_directory: &Path,
    ) -> Result<Self, KnowledgeError> {
        Self::open_persistent_with_authority(grant, id, paths, private_directory, private_directory)
    }
    /// Keep this authority directory current during index backup recovery.
    pub fn open_persistent_with_authority(
        grant: WorkspaceGrant,
        id: &str,
        paths: Vec<String>,
        private_directory: &Path,
        authority_directory: &Path,
    ) -> Result<Self, KnowledgeError> {
        Self::open_mode(
            grant,
            id,
            paths,
            Some((private_directory, authority_directory)),
        )
    }
    fn open_mode(
        grant: WorkspaceGrant,
        id: &str,
        paths: Vec<String>,
        private_directory: Option<(&Path, &Path)>,
    ) -> Result<Self, KnowledgeError> {
        if !valid_id(id) {
            return Err(KnowledgeError::Invalid);
        }
        let allowed = selected_paths(&paths)?.into_iter().cloned().collect();
        if grant.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let index = match private_directory {
            Some((dir, authority_dir)) => {
                persistence::open(dir, authority_dir, &grant, id, &allowed)?
            }
            None => persistence::transient()?,
        };
        Ok(Self {
            id: id.into(),
            paths: Arc::new(allowed),
            grant,
            index: Arc::new(Mutex::new(index)),
            revoked: Arc::new(AtomicBool::new(false)),
            publication: Arc::new(RwLock::new(())),
            journal_lease: None,
        })
    }
    /// Explicitly withdraw this collection and drain any publication already in progress.
    pub async fn withdraw(&self) -> Result<(), KnowledgeError> {
        // Registered names must retire through the same Journal transition as workspace consent.
        if self.journal_lease.is_some() {
            return Err(KnowledgeError::Invalid);
        }
        self.revoked.store(true, Ordering::Release);
        let exclusive = self.publication.clone().write_owned().await;
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || {
            let _exclusive = exclusive;
            let mut i = index.lock().map_err(|_| KnowledgeError::Storage)?;
            if i.private.is_some() {
                persistence::withdraw(&mut i)?;
            } else {
                i.db.execute_batch("DELETE FROM issued;DELETE FROM docs;DELETE FROM chunks;")?;
            }
            i.db.execute("DELETE FROM live", [])?;
            i.sources.clear();
            Ok(())
        })
        .await
        .map_err(|_| WorkspaceError::Interrupted)?
    }

    /// Recheck caller authority on each search and read. Keep the lease through final publication.
    pub async fn execute<T: Send + 'static>(
        &self,
        owner: &str,
        environment: &str,
        policy: &str,
        request: KnowledgeRequest,
        publish: impl FnOnce(KnowledgeResult) -> T + Send + 'static,
    ) -> Result<T, KnowledgeError> {
        if self.revoked.load(Ordering::Acquire) || self.grant.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let deadline = tokio::time::Instant::now() + self.grant.inner.deadline;
        let _publication = tokio::time::timeout_at(deadline, self.publication.read())
            .await
            .map_err(|_| WorkspaceError::Timeout)?;
        if self.revoked.load(Ordering::Acquire) {
            return Err(WorkspaceError::Revoked.into());
        }
        let slot = tokio::time::timeout_at(deadline, READ_SLOTS.clone().acquire_owned())
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        let permit =
            tokio::time::timeout_at(deadline, self.grant.permit(owner, environment, policy))
                .await
                .map_err(|_| WorkspaceError::Timeout)??;
        let this = self.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let _cancel = StopOnDrop(stop.clone());
        // The background task never publishes to the caller. Cancellation/timeout may retire an in-memory cache
        // but must not produce a late response or a late externally visible tool effect.
        let task = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let mut index = this.index.lock().map_err(|_| KnowledgeError::Storage)?;
            check_live(&this, &stop)?;
            let result = match request {
                KnowledgeRequest::KnowledgeSearch(q) => index.search(&this, q, &stop)?,
                KnowledgeRequest::KnowledgeRead(q) => index.read(&this, q, &stop)?,
            };
            check_live(&this, &stop)?;
            Ok::<_, KnowledgeError>((permit, result))
        });
        let result = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        let (lease, result) = result?;
        if self.revoked.load(Ordering::Acquire) || lease.inner.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(publish(result))
    }
}

// Keep the original file descriptor while its citations exist. An unlink followed by re-creation
// with identical text cannot inherit the old object's issued hits.
fn identity(file: &cap_std::fs::File) -> Result<(u64, u64, i128), KnowledgeError> {
    let m = file.metadata().map_err(WorkspaceError::Io)?;
    if !m.is_file() || MetadataExt::nlink(&m) != 1 || MetadataExt::ino(&m) == 0 {
        return Err(WorkspaceError::Forbidden.into());
    }
    #[cfg(unix)]
    let changed = i128::from(cap_fs_ext::OsMetadataExt::ctime(&m)) * 1_000_000_000
        + i128::from(cap_fs_ext::OsMetadataExt::ctime_nsec(&m));
    #[cfg(not(unix))]
    let changed = m
        .modified()
        .map_err(WorkspaceError::Io)?
        .into_std()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or_else(|e| -(e.duration().as_nanos() as i128));
    Ok((MetadataExt::dev(&m), MetadataExt::ino(&m), changed))
}
fn snapshot(root: &Dir, path: &str) -> Result<(Source, String), KnowledgeError> {
    let file = open_regular(root, path)?;
    let key = identity(&file)?;
    let mut reader = file.try_clone().map_err(WorkspaceError::Io)?.into_std();
    let (data, sha) = read_open_bytes(&mut reader, MAX_SOURCE, None)?;
    let text = String::from_utf8(data).map_err(|_| WorkspaceError::NotText)?;
    if text.contains('\0') {
        return Err(WorkspaceError::NotText.into());
    }
    if identity(&open_regular(root, path)?)? != key {
        return Err(WorkspaceError::VersionChanged.into());
    }
    Ok((
        Source {
            _file: file,
            identity: key,
            sha,
        },
        text,
    ))
}

fn chunks(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == text.len() {
            return None;
        }
        let mut end = (start + CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len()
            && let Some((i, c)) = text[start..end]
                .char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
            && i >= CHUNK_BYTES / 2
        {
            end = start + i + c.len_utf8();
        }
        let lo = start;
        start = end;
        Some((lo, end, &text[lo..end]))
    })
}
fn terms(text: &str) -> Vec<String> {
    let mut word = String::new();
    let mut tokens = Vec::new();
    let mut prior = None;
    for c in text
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
    {
        let han = matches!(c,'\u{3400}'..='\u{9fff}'|'\u{20000}'..='\u{323af}');
        if !han && c.is_alphanumeric() {
            prior = None;
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        if han {
            tokens.push(c.to_string());
            if let Some(prev) = prior {
                tokens.push(format!("{prev}{c}"));
            }
            prior = Some(c);
        } else {
            prior = None;
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}
fn citation(row: &rusqlite::Row<'_>, sha: String) -> rusqlite::Result<Hit> {
    Ok(Hit {
        hit_id: row.get(0)?,
        path: row.get(1)?,
        source_sha256: sha,
        start_byte: row.get::<_, i64>(2)?.to_string(),
        end_byte: row.get::<_, i64>(3)?.to_string(),
    })
}

impl Index {
    fn initialized(
        db: Connection,
        private: Option<persistence::PrivateIndex>,
    ) -> Result<Self, KnowledgeError> {
        db.execute_batch("CREATE TEMP TABLE live(path TEXT PRIMARY KEY,sha TEXT NOT NULL); CREATE INDEX temp.live_by_hash ON live(sha,path);")?;
        Ok(Self {
            db,
            private,
            sources: BTreeMap::new(),
        })
    }
    fn path_key(&self, path: &str) -> String {
        self.private
            .as_ref()
            .map_or_else(|| path.to_owned(), |idx| idx.tag(b"path\0", path))
    }
    fn encode_term(private: Option<&persistence::PrivateIndex>, term: String) -> String {
        match private {
            Some(idx) => idx.tag(b"term\0", &term),
            None => term,
        }
    }

    fn forget(&mut self, path: &str) -> Result<(), KnowledgeError> {
        use rusqlite::OptionalExtension;
        let key = self.path_key(path);
        let tx = self.db.transaction()?;
        let old: Option<String> = tx
            .query_row("SELECT sha FROM docs WHERE path=?", [&key], |r| r.get(0))
            .optional()?;
        if let Some(sha) = old {
            tx.execute("DELETE FROM issued WHERE path=?", [&key])?;
            tx.execute("DELETE FROM docs WHERE path=?", [&key])?;
            Self::purge_unreferenced(&tx, &sha)?;
        }
        tx.execute("DELETE FROM live WHERE path=?", [path])?;
        tx.commit()?;
        self.sources.remove(path);
        Ok(())
    }
    fn purge_unreferenced(tx: &rusqlite::Transaction<'_>, sha: &str) -> Result<(), KnowledgeError> {
        tx.execute(
            "DELETE FROM chunks WHERE sha=?1 AND NOT EXISTS(SELECT 1 FROM docs WHERE sha=?1)",
            [sha],
        )?;
        Ok(())
    }
    fn refresh(
        &mut self,
        c: &KnowledgeCollection,
        path: &str,
        stop: &AtomicBool,
    ) -> Result<(), KnowledgeError> {
        check_live(c, stop)?;
        let (source, text) = match snapshot(&c.grant.inner.directory, path) {
            Ok(value) => value,
            Err(err) => {
                self.forget(path)?;
                match err {
                    KnowledgeError::Workspace(
                        WorkspaceError::NotText
                        | WorkspaceError::Forbidden
                        | WorkspaceError::FileTooLarge,
                    ) => return Ok(()),
                    KnowledgeError::Workspace(WorkspaceError::Io(e))
                        if matches!(
                            e.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::PermissionDenied
                                | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(());
                    }
                    _ => return Err(err),
                }
            }
        };
        if let Some(old) = self.sources.get(path)
            && old.sha == source.sha
            && old.identity == source.identity
        {
            return Ok(());
        }
        use rusqlite::OptionalExtension;
        let key = self.path_key(path);
        let birth = source
            ._file
            .metadata()
            .map_err(WorkspaceError::Io)?
            .created()
            .ok()
            .and_then(|v| v.into_std().duration_since(UNIX_EPOCH).ok())
            .map(|v| v.as_nanos().to_string());
        if self.private.is_some() && birth.is_none() {
            self.forget(path)?;
            return Err(WorkspaceError::PersistentIdentityUnavailable.into());
        }
        let stamp = request_digest(
            "local-knowledge-file-stamp.v1",
            &(
                source.identity.0,
                source.identity.1,
                source.identity.2.to_string(),
                birth,
                &source.sha,
            ),
        )
        .map_err(|_| KnowledgeError::Invalid)?;
        let tx = self.db.transaction()?;
        let prior: Option<(String, String)> = tx
            .query_row("SELECT sha,stamp FROM docs WHERE path=?", [&key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let known: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM docs WHERE sha=?)",
            [&source.sha],
            |r| r.get(0),
        )?;
        if prior
            .as_ref()
            .is_none_or(|(s, t)| s != &source.sha || t != &stamp)
        {
            tx.execute("DELETE FROM issued WHERE path=?", [&key])?;
            tx.execute("INSERT INTO docs(path,sha,stamp) VALUES(?,?,?) ON CONFLICT(path) DO UPDATE SET sha=excluded.sha,stamp=excluded.stamp",params![key,source.sha,stamp])?;
            if let Some((old, _)) = prior
                && old != source.sha
            {
                Self::purge_unreferenced(&tx, &old)?;
            }
        }
        tx.execute("INSERT INTO live(path,sha) VALUES(?,?) ON CONFLICT(path) DO UPDATE SET sha=excluded.sha",params![path,source.sha])?;
        if !known {
            let mut add = tx.prepare("INSERT INTO chunks(terms,sha,lo,hi) VALUES(?,?,?,?)")?;
            for (lo, hi, chunk) in chunks(&text) {
                check_live(c, stop)?;
                add.execute(params![
                    terms(chunk)
                        .into_iter()
                        .map(|t| Self::encode_term(self.private.as_ref(), t))
                        .collect::<Vec<_>>()
                        .join(" "),
                    source.sha,
                    lo as i64,
                    hi as i64
                ])?;
            }
        }
        tx.commit()?;
        self.sources.insert(path.into(), source);
        Ok(())
    }
    fn search(
        &mut self,
        c: &KnowledgeCollection,
        q: Search,
        stop: &AtomicBool,
    ) -> Result<KnowledgeResult, KnowledgeError> {
        let limit = q.limit.unwrap_or(8);
        if q.collection_id != c.id {
            return Err(WorkspaceError::Forbidden.into());
        }
        if q.query.len() > 256 || !(1..=MAX_HITS).contains(&limit) {
            return Err(KnowledgeError::Invalid);
        }
        if q.path.as_ref().is_some_and(|p| !c.paths.contains(p)) {
            return Err(WorkspaceError::Forbidden.into());
        }
        let words = terms(&q.query).into_iter().collect::<BTreeSet<_>>();
        if words.is_empty() || words.len() > 48 {
            return Err(KnowledgeError::Invalid);
        }
        let expression = words
            .iter()
            .cloned()
            .map(|w| format!("\"{}\"", Self::encode_term(self.private.as_ref(), w)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let refresh_all = self.private.is_some();
        let mut unavailable_sources = 0;
        for path in c
            .paths
            .iter()
            .filter(|p| refresh_all || q.path.as_ref().is_none_or(|f| f == *p))
        {
            self.refresh(c, path, stop)?;
            if !self.sources.contains_key(path) && q.path.as_ref().is_none_or(|f| f == path) {
                unavailable_sources += 1;
            }
        }
        let tx = self.db.transaction()?;
        let mut found = {
            // Apply the source filter before ranking/paging. Identical authorized versions share one posting list.
            let mut select=tx.prepare("WITH visible AS (
                SELECT sha,min(path) AS path FROM live WHERE (?1 IS NULL OR path=?1) GROUP BY sha)
                SELECT visible.path,chunks.sha,chunks.lo,chunks.hi FROM chunks JOIN visible ON visible.sha=chunks.sha
                WHERE chunks MATCH ?2 ORDER BY rank,visible.path,chunks.lo LIMIT ?3")?;
            select
                .query_map(params![q.path, expression, limit as i64 + 1], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let truncated = found.len() > limit;
        found.truncate(limit);
        let mut verified = BTreeMap::new();
        let mut hits = Vec::with_capacity(found.len());
        for (path, sha, lo, hi) in found {
            check_live(c, stop)?;
            if !verified.contains_key(&path) {
                let (live, text) =
                    snapshot(&c.grant.inner.directory, &path).map_err(|_| KnowledgeError::Stale)?;
                let old = self.sources.get(&path).ok_or(KnowledgeError::Stale)?;
                if old.identity != live.identity || old.sha != sha || live.sha != sha {
                    return Err(KnowledgeError::Stale);
                }
                verified.insert(path.clone(), text);
            }
            // A persistent posting may be corrupt. Verify the claimed range and literal terms against
            // the live document before issuing any citation, even when the file digest is current.
            let text = verified.get(&path).ok_or(KnowledgeError::Stale)?;
            let chunk = chunks(text)
                .find(|(l, h, _)| i64::try_from(*l) == Ok(lo) && i64::try_from(*h) == Ok(hi))
                .map(|(_, _, v)| v)
                .ok_or(KnowledgeError::Stale)?;
            let present = terms(chunk).into_iter().collect::<BTreeSet<_>>();
            if !words.is_subset(&present) {
                return Err(KnowledgeError::Stale);
            }
            use rusqlite::OptionalExtension;
            let path_key = self
                .private
                .as_ref()
                .map_or_else(|| path.clone(), |idx| idx.tag(b"path\0", &path));
            let existing = tx
                .query_row(
                    "SELECT hit FROM issued WHERE path=? AND sha=? AND lo=? AND hi=?",
                    params![path_key, sha, lo, hi],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let hit_id = if let Some(id) = existing {
                id
            } else {
                let id = Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO issued(hit,path,sha,lo,hi) VALUES(?,?,?,?,?)",
                    params![id, path_key, sha, lo, hi],
                )?;
                id
            };
            hits.push(Hit {
                hit_id,
                path,
                source_sha256: sha,
                start_byte: lo.to_string(),
                end_byte: hi.to_string(),
            });
        }
        check_live(c, stop)?;
        tx.commit()?;
        Ok(KnowledgeResult::KnowledgeSearch {
            collection_id: c.id.clone(),
            hits,
            truncated,
            unavailable_sources,
        })
    }
    fn read(
        &mut self,
        c: &KnowledgeCollection,
        q: Read,
        stop: &AtomicBool,
    ) -> Result<KnowledgeResult, KnowledgeError> {
        use rusqlite::OptionalExtension;
        if q.collection_id != c.id {
            return Err(WorkspaceError::Forbidden.into());
        }
        if Uuid::parse_str(&q.hit_id).is_err() || !valid_digest(&q.expected_sha256) {
            return Err(KnowledgeError::Invalid);
        }
        let mut hit = self
            .db
            .query_row(
                "SELECT hit,path,lo,hi,sha FROM issued WHERE hit=? AND sha=?",
                params![q.hit_id, q.expected_sha256],
                |r| citation(r, r.get(4)?),
            )
            .optional()?
            .ok_or(KnowledgeError::Stale)?;
        // An on-disk path identifier is never interpreted as a filesystem path.
        let path = c
            .paths
            .iter()
            .find(|p| self.path_key(p) == hit.path)
            .ok_or(KnowledgeError::Stale)?
            .clone();
        hit.path = path;
        if !self.sources.contains_key(&hit.path) {
            self.refresh(c, &hit.path, stop)?;
            let retained: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM issued WHERE hit=? AND sha=?)",
                params![q.hit_id, q.expected_sha256],
                |r| r.get(0),
            )?;
            if !retained {
                return Err(KnowledgeError::Stale);
            }
        }
        let old = self.sources.get(&hit.path).ok_or(KnowledgeError::Stale)?;
        let (file, text) = match snapshot(&c.grant.inner.directory, &hit.path) {
            Ok(v) => v,
            Err(_) => {
                self.forget(&hit.path)?;
                return Err(KnowledgeError::Stale);
            }
        };
        if file.sha != hit.source_sha256 || old.identity != file.identity {
            self.forget(&hit.path)?;
            return Err(KnowledgeError::Stale);
        }
        let lo = hit
            .start_byte
            .parse::<usize>()
            .map_err(|_| KnowledgeError::Storage)?;
        let hi = hit
            .end_byte
            .parse::<usize>()
            .map_err(|_| KnowledgeError::Storage)?;
        let chunk = chunks(&text)
            .find(|(l, h, _)| *l == lo && *h == hi)
            .map(|(_, _, data)| data)
            .ok_or(KnowledgeError::Stale)?;
        let start = q.start_byte.unwrap_or(0);
        let end = q.end_byte.unwrap_or(chunk.len());
        if start >= end {
            return Err(KnowledgeError::Invalid);
        }
        let content = chunk.get(start..end).ok_or(KnowledgeError::Invalid)?;
        Ok(KnowledgeResult::KnowledgeRead {
            citation: hit,
            start_byte: (lo + start).to_string(),
            end_byte: (lo + end).to_string(),
            text: content.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tempfile::tempdir;
    fn search() -> KnowledgeRequest {
        KnowledgeRequest::KnowledgeSearch(Search {
            collection_id: "manual".into(),
            query: "cobalt".into(),
            limit: Some(1),
            path: None,
        })
    }
    #[tokio::test]
    async fn owner_and_revocation_gate_publication_and_drain() {
        let base = tempdir().expect("fixture");
        let root = base.path().join("documents");
        let private = base.path().join("private");
        std::fs::create_dir(&root).expect("root");
        std::fs::create_dir(&private).expect("private");
        std::fs::write(root.join("note.txt"), "cobalt").expect("source file");
        let grant =
            WorkspaceGrant::open("owner", "grant", "policy", &root, &[private]).expect("grant");
        let c = KnowledgeCollection::open(grant.clone(), "manual", vec!["note.txt".into()])
            .expect("selection");
        let published = Arc::new(AtomicUsize::new(0));
        let count = published.clone();
        assert!(
            c.execute(
                "other",
                grant.environment(),
                "policy",
                search(),
                move |_| count.fetch_add(1, Ordering::AcqRel)
            )
            .await
            .is_err()
        );
        let count = published.clone();
        c.execute(
            "owner",
            grant.environment(),
            "policy",
            search(),
            move |_| count.fetch_add(1, Ordering::AcqRel),
        )
        .await
        .expect("first query");
        grant.request_revoke();
        grant.wait_drained().await;
        let count = published.clone();
        assert!(
            c.execute(
                "owner",
                grant.environment(),
                "policy",
                search(),
                move |_| count.fetch_add(1, Ordering::AcqRel)
            )
            .await
            .is_err()
        );
        assert_eq!(published.load(Ordering::Acquire), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn acknowledged_revocation_waits_for_the_result_to_leave_the_host() {
        let base = tempdir().expect("fixture");
        let workspace = base.path().join("w");
        let private = base.path().join("p");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&private).expect("private");
        std::fs::write(workspace.join("one.txt"), "cobalt").expect("doc");
        let grant = WorkspaceGrant::open("owner", "grant", "policy", &workspace, &[private])
            .expect("grant");
        let c = KnowledgeCollection::open(grant.clone(), "manual", vec!["one.txt".into()])
            .expect("selection");
        let environment = grant.environment().to_owned();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let (release, finish) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            c.execute("owner", &environment, "policy", search(), move |_| {
                entered.send(()).expect("entered");
                finish
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release publication");
            })
            .await
        });
        wait.await.expect("result acquired");
        grant.request_revoke();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), grant.wait_drained())
                .await
                .is_err()
        );
        release.send(()).expect("release");
        task.await.expect("join").expect("original delivery");
        grant.wait_drained().await;
    }
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn saved_withdrawal_drains_current_delivery_and_is_sticky_after_reopen() {
        use std::os::unix::fs::DirBuilderExt;
        let base = tempdir().expect("fixture");
        let work = base.path().join("docs");
        let private = base.path().join("private");
        let unrelated = base.path().join("other");
        for dir in [&work, &private, &unrelated] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .expect("private directory");
        }
        std::fs::write(work.join("note.txt"), "cobalt").expect("fixture");
        let grant = WorkspaceGrant::open(
            "owner",
            "stable-grant",
            "policy",
            &work,
            std::slice::from_ref(&private),
        )
        .expect("grant");
        assert!(matches!(
            KnowledgeCollection::open_persistent(
                grant.clone(),
                "manual",
                vec!["note.txt".into()],
                &unrelated
            ),
            Err(KnowledgeError::Workspace(WorkspaceError::Forbidden))
        ));
        let open = || {
            KnowledgeCollection::open_persistent(
                grant.clone(),
                "manual",
                vec!["note.txt".into()],
                &private,
            )
        };
        let collection = open().expect("persistent collection");
        let serving = collection.clone();
        let access = grant.clone();
        let (ready, entered) = tokio::sync::oneshot::channel();
        let (release, finish) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            serving
                .execute(
                    access.owner(),
                    access.environment(),
                    access.policy(),
                    search(),
                    move |_| {
                        ready.send(()).expect("enter");
                        finish.recv_timeout(Duration::from_secs(10)).expect("leave");
                    },
                )
                .await
        });
        entered.await.expect("entered");
        let retiring = collection.clone();
        let withdrawal = tokio::spawn(async move { retiring.withdraw().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !collection.revoked.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("withdrawal began");
        assert!(!withdrawal.is_finished());
        assert!(matches!(
            collection
                .execute(
                    grant.owner(),
                    grant.environment(),
                    grant.policy(),
                    search(),
                    |_| ()
                )
                .await,
            Err(KnowledgeError::Workspace(WorkspaceError::Revoked))
        ));
        release.send(()).expect("unblock");
        task.await.expect("join").expect("initial publication");
        withdrawal.await.expect("join").expect("withdrawn");
        drop(collection);
        assert!(matches!(
            open(),
            Err(KnowledgeError::Workspace(WorkspaceError::Revoked))
        ));
        assert!(
            !grant.is_revoked(),
            "collection withdrawal should not revoke other workspace uses"
        );
    }

    #[test]
    fn chunk_ranges_reconstruct_unicode_without_losing_a_byte() {
        let text = "公共 transport 文档 🟠\n".repeat(165);
        let parts = chunks(&text).collect::<Vec<_>>();
        assert!(parts.len() > 1);
        let mut last = 0;
        for (start, end, chunk) in parts {
            assert_eq!(start, last);
            assert_eq!(Some(chunk), text.get(start..end));
            assert!(end - start <= CHUNK_BYTES);
            last = end;
        }
        assert_eq!(last, text.len());
    }
}
