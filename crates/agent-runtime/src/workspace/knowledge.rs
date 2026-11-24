//! Session-local text retrieval over an explicit file selection. No terms or document bytes go to disk.
use super::*;
use cap_fs_ext::MetadataExt;
use rusqlite::{Connection, params};
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
    if collection.grant.is_revoked() {
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
}
#[derive(Clone)]
pub struct KnowledgeCollection {
    id: String,
    paths: Arc<BTreeSet<String>>,
    grant: WorkspaceGrant,
    index: Arc<Mutex<Index>>,
}
impl KnowledgeCollection {
    /// Only the trusted host chooses files. A new grant/selection starts a new citation namespace.
    pub fn open(
        grant: WorkspaceGrant,
        id: &str,
        paths: Vec<String>,
    ) -> Result<Self, KnowledgeError> {
        if !valid_id(id) || paths.is_empty() || paths.len() > MAX_DOCS {
            return Err(KnowledgeError::Invalid);
        }
        let mut allowed = BTreeSet::new();
        for path in paths {
            components(&path, false)?;
            if !allowed.insert(path) {
                return Err(KnowledgeError::Invalid);
            }
        }
        if grant.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let db = Connection::open_in_memory()?;
        db.execute_batch("PRAGMA temp_store=MEMORY;
          CREATE TABLE docs(path TEXT PRIMARY KEY, sha TEXT NOT NULL);
          CREATE INDEX docs_by_hash ON docs(sha,path);
          CREATE VIRTUAL TABLE chunks USING fts5(terms, sha UNINDEXED, lo UNINDEXED, hi UNINDEXED);
          CREATE TABLE issued(hit TEXT PRIMARY KEY, path TEXT NOT NULL, sha TEXT NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL, UNIQUE(path,sha,lo,hi));")?;
        Ok(Self {
            id: id.into(),
            paths: Arc::new(allowed),
            grant,
            index: Arc::new(Mutex::new(Index {
                db,
                sources: BTreeMap::new(),
            })),
        })
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
        if self.grant.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        let deadline = tokio::time::Instant::now() + self.grant.inner.deadline;
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
                KnowledgeRequest::KnowledgeRead(q) => index.read(&this, q)?,
            };
            check_live(&this, &stop)?;
            Ok::<_, KnowledgeError>((permit, result))
        });
        let result = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        let (lease, result) = result?;
        if lease.inner.is_revoked() {
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
    for c in text.nfkc().flat_map(char::to_lowercase) {
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
    fn forget(&mut self, path: &str) -> Result<(), KnowledgeError> {
        let Some(old) = self.sources.get(path) else {
            return Ok(());
        };
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM issued WHERE path=?", [path])?;
        tx.execute("DELETE FROM docs WHERE path=?", [path])?;
        Self::purge_unreferenced(&tx, &old.sha)?;
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
        let tx = self.db.transaction()?;
        let known: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM docs WHERE sha=?)",
            [&source.sha],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM issued WHERE path=?", [path])?;
        tx.execute("INSERT INTO docs(path,sha) VALUES(?,?) ON CONFLICT(path) DO UPDATE SET sha=excluded.sha",params![path,source.sha])?;
        if let Some(old) = self.sources.get(path)
            && old.sha != source.sha
        {
            Self::purge_unreferenced(&tx, &old.sha)?;
        }
        if !known {
            let mut add = tx.prepare("INSERT INTO chunks(terms,sha,lo,hi) VALUES(?,?,?,?)")?;
            for (lo, hi, chunk) in chunks(&text) {
                check_live(c, stop)?;
                add.execute(params![
                    terms(chunk).join(" "),
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
            .into_iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut unavailable_sources = 0;
        for path in c
            .paths
            .iter()
            .filter(|p| q.path.as_ref().is_none_or(|f| f == *p))
        {
            self.refresh(c, path, stop)?;
            if !self.sources.contains_key(path) {
                unavailable_sources += 1;
            }
        }
        let tx = self.db.transaction()?;
        let mut found = {
            // Apply the source filter before ranking/paging. Identical authorized versions share one posting list.
            let mut select=tx.prepare("WITH visible AS (
                SELECT sha,min(path) AS path FROM docs WHERE (?1 IS NULL OR path=?1) GROUP BY sha)
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
        let mut verified = BTreeSet::new();
        let mut hits = Vec::with_capacity(found.len());
        for (path, sha, lo, hi) in found {
            check_live(c, stop)?;
            if verified.insert(path.clone()) {
                let (live, _) =
                    snapshot(&c.grant.inner.directory, &path).map_err(|_| KnowledgeError::Stale)?;
                let old = self.sources.get(&path).ok_or(KnowledgeError::Stale)?;
                if old.identity != live.identity || sha != live.sha {
                    return Err(KnowledgeError::Stale);
                }
            }
            use rusqlite::OptionalExtension;
            let existing = tx
                .query_row(
                    "SELECT hit FROM issued WHERE path=? AND sha=? AND lo=? AND hi=?",
                    params![path, sha, lo, hi],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let hit_id = if let Some(id) = existing {
                id
            } else {
                let id = Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO issued(hit,path,sha,lo,hi) VALUES(?,?,?,?,?)",
                    params![id, path, sha, lo, hi],
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
    ) -> Result<KnowledgeResult, KnowledgeError> {
        use rusqlite::OptionalExtension;
        if q.collection_id != c.id {
            return Err(WorkspaceError::Forbidden.into());
        }
        if Uuid::parse_str(&q.hit_id).is_err() || !valid_digest(&q.expected_sha256) {
            return Err(KnowledgeError::Invalid);
        }
        let hit = self
            .db
            .query_row(
                "SELECT hit,path,lo,hi,sha FROM issued WHERE hit=? AND sha=?",
                params![q.hit_id, q.expected_sha256],
                |r| citation(r, r.get(4)?),
            )
            .optional()?
            .ok_or(KnowledgeError::Stale)?;
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
        let chunk = text.get(lo..hi).ok_or(KnowledgeError::Stale)?;
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
