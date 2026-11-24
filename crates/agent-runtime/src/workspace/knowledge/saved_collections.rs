//! Trusted local host operations. Reopening always requires the current session's OS grant and file selection.
use super::*;
use crate::{Journal, workspaces::WorkspaceStoreError};
use rusqlite::OptionalExtension;

#[derive(Debug, thiserror::Error)]
pub enum SavedKnowledgeError {
    #[error(transparent)]
    Knowledge(#[from] KnowledgeError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceStoreError),
    #[error("saved collection is withdrawn")]
    Retired,
    #[error("saved collection metadata unavailable")]
    Metadata(#[from] rusqlite::Error),
}

impl Journal {
    fn saved_workspace_version(
        &self,
        workspace: &str,
        grant: &WorkspaceGrant,
    ) -> Result<String, SavedKnowledgeError> {
        let view = self.workspace(grant.owner(), workspace)?;
        let gate = self.workspace_origin_gate(workspace);
        if !view.active
            || grant.is_revoked()
            || !grant.has_persistent_identity()
            || grant.environment() != view.environment
            || grant.resource_identity() != view.resource_identity
            || !grant
                .inner
                .origin_gate
                .as_ref()
                .is_some_and(|origin| Arc::ptr_eq(origin, &gate))
        {
            return Err(WorkspaceStoreError::Revoked.into());
        }
        Ok(view.version)
    }

    /// The name is actor-supplied. File names and content are never journalled by this registration.
    pub fn open_saved_knowledge(
        &mut self,
        workspace: &str,
        grant: &WorkspaceGrant,
        name: &str,
        paths: Vec<String>,
    ) -> Result<KnowledgeCollection, SavedKnowledgeError> {
        if !valid_id(name) {
            return Err(KnowledgeError::Invalid.into());
        }
        selected_paths(&paths)?;
        let version = self.saved_workspace_version(workspace, grant)?;
        // Insert the recovery record before creating either SQLite sidecar store. A crash during
        // initialization can be retried or retired without losing its stable private namespace.
        self.connection.execute("INSERT INTO saved_knowledge_collections(workspace_id,name,store_id,permission_version,state)
            VALUES(?,?,?,?,'active') ON CONFLICT(workspace_id,name) DO NOTHING",
            params![workspace,name,Uuid::new_v4().to_string(),version])?;
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT store_id FROM saved_knowledge_collections
            WHERE workspace_id=? AND name=? AND permission_version=? AND state='active'",
                params![workspace, name, version],
                |r| r.get(0),
            )
            .optional()?;
        let id = id.ok_or(SavedKnowledgeError::Retired)?;
        let mut collection =
            KnowledgeCollection::open_persistent(grant.clone(), &id, paths, &self.private_root)?;
        collection.journal_lease = Some(Arc::new(self._host_lock.effect_lease()));
        self.saved_knowledge_signals
            .retain(|_, signal| signal.strong_count() > 0);
        self.saved_knowledge_signals
            .insert(id, Arc::downgrade(&collection.revoked));
        Ok(collection)
    }

    /// Trusted UI lookup. A reopened Journal alone does not grant access to private collection names.
    pub fn saved_knowledge_names(
        &self,
        workspace: &str,
        grant: &WorkspaceGrant,
    ) -> Result<Vec<String>, SavedKnowledgeError> {
        let version = self.saved_workspace_version(workspace, grant)?;
        let mut stmt = self.connection.prepare(
            "SELECT name FROM saved_knowledge_collections WHERE
            workspace_id=? AND permission_version=? AND state='active' ORDER BY name",
        )?;
        Ok(stmt
            .query_map(params![workspace, version], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Mark first, then wipe. If a live call is still draining, the durable queue owns the retry.
    pub fn retire_saved_knowledge(
        &mut self,
        owner: &str,
        workspace: &str,
        name: &str,
    ) -> Result<(), SavedKnowledgeError> {
        self.workspace(owner, workspace)?;
        if !valid_id(name) {
            return Err(KnowledgeError::Invalid.into());
        }
        let (id,state):(String,String)=self.connection.query_row(
            "SELECT store_id,state FROM saved_knowledge_collections WHERE workspace_id=? AND name=?",
            params![workspace,name],|row|Ok((row.get(0)?,row.get(1)?))).optional()?.ok_or(WorkspaceStoreError::NotFound)?;
        if state == "retired" {
            return Ok(());
        }
        self.connection.execute("UPDATE saved_knowledge_collections SET state='retiring' WHERE store_id=? AND state='active'",[&id])?;
        self.finish_saved_knowledge(owner, &id)
    }

    fn finish_saved_knowledge(&mut self, owner: &str, id: &str) -> Result<(), SavedKnowledgeError> {
        if let Some(signal) = self
            .saved_knowledge_signals
            .get(id)
            .and_then(std::sync::Weak::upgrade)
        {
            signal.store(true, Ordering::Release);
        }
        persistence::retire_closed(&self.private_root, &self.private_root, owner, id, true)?;
        self.connection.execute("UPDATE saved_knowledge_collections SET state='retired' WHERE store_id=? AND state='retiring'",[id])?;
        self.saved_knowledge_signals.remove(id);
        Ok(())
    }

    /// Run after dropping closed sessions, on host startup, and after workspace withdrawal. Process every
    /// independent pending retirement, preserving a failed row for another attempt on the next call.
    pub fn reconcile_saved_knowledge(&mut self) -> Result<(), SavedKnowledgeError> {
        let pending = {
            let mut stmt=self.connection.prepare("SELECT w.owner,s.store_id FROM saved_knowledge_collections s
                JOIN local_workspaces w USING(workspace_id) WHERE s.state='retiring' ORDER BY s.store_id")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut result = Ok(());
        for (owner, id) in pending {
            if let Err(error) = self.finish_saved_knowledge(&owner, &id) {
                result = Err(error);
            }
        }
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
    struct Fixture {
        _tmp: tempfile::TempDir,
        db: PathBuf,
        docs: PathBuf,
        private: PathBuf,
        id: String,
    }
    impl Fixture {
        fn new() -> TestResult<(Self, Journal, WorkspaceGrant)> {
            let tmp = tempfile::tempdir()?;
            let docs = tmp.path().join("manuals");
            let private = tmp.path().join("private");
            fs::create_dir(&docs)?;
            fs::create_dir(&private)?;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
            fs::write(docs.join("manual.txt"), "Orchard opens in April")?;
            let db = private.join("journal.sqlite");
            let mut journal = Journal::open(&db)?;
            let id = journal
                .authorize_workspace("owner", "consent", &docs, "read")?
                .workspace_id;
            let grant = journal.open_workspace("owner", &id)?;
            Ok((
                Self {
                    _tmp: tmp,
                    db,
                    docs,
                    private,
                    id,
                },
                journal,
                grant,
            ))
        }
    }
    fn paths() -> Vec<String> {
        vec!["manual.txt".into()]
    }
    async fn search(c: &KnowledgeCollection) -> Result<Vec<Hit>, KnowledgeError> {
        let query = KnowledgeRequest::KnowledgeSearch(Search {
            collection_id: c.id.clone(),
            query: "Orchard".into(),
            path: None,
            limit: None,
        });
        c.execute(
            c.grant.owner(),
            c.grant.environment(),
            c.grant.policy(),
            query,
            |r| match r {
                KnowledgeResult::KnowledgeSearch { hits, .. } => hits,
                _ => panic!("expected search"),
            },
        )
        .await
    }
    #[test]
    fn upgrades_schema_35_without_reauthorizing_or_changing_existing_workspace_consent()
    -> TestResult {
        let (f, j, g) = Fixture::new()?;
        let before = j.workspace("owner", &f.id)?;
        drop(g);
        drop(j);
        // Reconstruct the direct predecessor on a test-owned database. v36 adds exactly this table and trigger.
        let connection = Connection::open(&f.db)?;
        connection.execute_batch("DROP TRIGGER retire_workspace_saved_knowledge; DROP TABLE saved_knowledge_collections; PRAGMA user_version=35;")?;
        drop(connection);
        let mut j = Journal::open(&f.db)?;
        let after = j.workspace("owner", &f.id)?;
        assert_eq!(
            (before.version, before.environment),
            (after.version, after.environment)
        );
        let g = j.open_workspace("owner", &f.id)?;
        assert!(j.saved_knowledge_names(&f.id, &g)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn durable_name_needs_fresh_journal_grant_and_keeps_workspace_revoke_across_restart()
    -> TestResult {
        let (f, mut j, g) = Fixture::new()?;
        let forged = WorkspaceGrant::open(
            "owner",
            &f.id,
            "read",
            &f.docs,
            std::slice::from_ref(&f.private),
        )?;
        assert_eq!(forged.environment(), g.environment());
        assert!(
            j.open_saved_knowledge(&f.id, &forged, "notes", paths())
                .is_err()
        );
        let c = j.open_saved_knowledge(&f.id, &g, "notes", paths())?;
        let found = search(&c).await?;
        assert_eq!(found.len(), 1);
        assert!(matches!(c.withdraw().await, Err(KnowledgeError::Invalid)));
        let old_id = c.id.clone();
        drop(j);
        assert!(matches!(
            Journal::open(&f.db),
            Err(crate::JournalError::Busy)
        ));
        drop(c);
        let mut j = Journal::open(&f.db)?;
        assert!(j.saved_knowledge_names(&f.id, &g).is_err());
        let fresh = j.open_workspace("owner", &f.id)?;
        assert_eq!(j.saved_knowledge_names(&f.id, &fresh)?, ["notes"]);
        assert!(
            j.open_saved_knowledge(&f.id, &fresh, "notes", vec!["other.txt".into()])
                .is_err()
        );
        let c = j.open_saved_knowledge(&f.id, &fresh, "notes", paths())?;
        assert_eq!(c.id, old_id);
        assert_eq!(search(&c).await?[0].hit_id, found[0].hit_id);
        let view = j.revoke_workspace("owner", &f.id, "1")?;
        assert!(!view.active);
        assert!(search(&c).await.is_err());
        assert!(j.reconcile_saved_knowledge().is_err());
        drop(c);
        drop(j);
        let mut j = Journal::open(&f.db)?;
        j.reconcile_saved_knowledge()?;
        let state: String = j.connection.query_row(
            "SELECT state FROM saved_knowledge_collections WHERE store_id=?",
            [&old_id],
            |r| r.get(0),
        )?;
        assert_eq!(state, "retired");
        assert!(
            KnowledgeCollection::open_persistent(forged, &old_id, paths(), &f.private).is_err()
        );
        let new = j
            .authorize_workspace("owner", "new-consent", &f.docs, "read")?
            .workspace_id;
        let grant = j.open_workspace("owner", &new)?;
        let replacement = j.open_saved_knowledge(&new, &grant, "notes", paths())?;
        assert_ne!(search(&replacement).await?[0].hit_id, found[0].hit_id);
        Ok(())
    }
    #[tokio::test]
    async fn named_withdrawal_drains_only_one_collection_and_recovers_incomplete_initialization()
    -> TestResult {
        let (f, mut j, g) = Fixture::new()?;
        let a = j.open_saved_knowledge(&f.id, &g, "retire", paths())?;
        let b = j.open_saved_knowledge(&f.id, &g, "keep", paths())?;
        assert!(j.retire_saved_knowledge("other", &f.id, "retire").is_err());
        assert_eq!(search(&a).await?.len(), 1);
        assert!(matches!(
            j.retire_saved_knowledge("owner", &f.id, "retire"),
            Err(SavedKnowledgeError::Knowledge(KnowledgeError::Busy))
        ));
        assert!(search(&a).await.is_err());
        assert_eq!(search(&b).await?.len(), 1);
        // This is the intermediate durable state after registration and before creation of either store.
        j.connection.execute("INSERT INTO saved_knowledge_collections VALUES(?,'interrupted','cold-id',1,'retiring')",[&f.id])?;
        assert!(j.reconcile_saved_knowledge().is_err());
        let state: String = j.connection.query_row(
            "SELECT state FROM saved_knowledge_collections WHERE store_id='cold-id'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(state, "retired");
        assert!(
            KnowledgeCollection::open_persistent(g.clone(), "cold-id", paths(), &f.private)
                .is_err()
        );
        drop(a);
        j.reconcile_saved_knowledge()?;
        j.retire_saved_knowledge("owner", &f.id, "retire")?;
        assert!(matches!(
            j.open_saved_knowledge(&f.id, &g, "retire", paths()),
            Err(SavedKnowledgeError::Retired)
        ));
        assert_eq!(j.saved_knowledge_names(&f.id, &g)?, ["keep"]);
        Ok(())
    }
}
