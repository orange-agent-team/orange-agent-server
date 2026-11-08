use super::*;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreationConsentView {
    pub consent_id: String,
    pub owner: String,
    pub canonical_path: String,
    pub resource_identity: String,
    pub environment: String,
    pub policy: String,
    pub active: bool,
    pub version: String,
}
#[derive(Clone)]
pub struct CreationRevoker {
    flag: Arc<AtomicBool>,
}
impl CreationRevoker {
    /// Immediate stop signal only. A successful durable revoke is still needed
    /// before the Host acknowledges consent removal or releases an OS scope.
    pub fn request_revoke(&self) {
        self.flag.store(true, Ordering::Release);
    }
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CreationConsentView> {
    Ok(CreationConsentView {
        consent_id: r.get("consent_id")?,
        owner: r.get("owner")?,
        canonical_path: r.get("canonical_path")?,
        resource_identity: r.get("resource_identity")?,
        environment: r.get("environment")?,
        policy: r.get("policy")?,
        active: r.get("active")?,
        version: r.get::<_, i64>("version")?.to_string(),
    })
}
impl Journal {
    pub fn creation_consent(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<CreationConsentView, CreateFileError> {
        self.connection
            .query_row(
                "SELECT * FROM creation_consents WHERE owner=? AND consent_id=?",
                params![owner, id],
                row,
            )
            .optional()
            .map_err(JournalError::from)?
            .ok_or(WorkspaceError::Forbidden.into())
    }
    pub fn creation_consents(
        &self,
        owner: &str,
    ) -> Result<Vec<CreationConsentView>, CreateFileError> {
        if !valid_id(owner) {
            return Err(JournalError::Invalid.into());
        }
        let mut query = self
            .connection
            .prepare("SELECT * FROM creation_consents WHERE owner=? ORDER BY consent_id LIMIT 1000")
            .map_err(JournalError::from)?;
        Ok(query
            .query_map([owner], row)
            .map_err(JournalError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(JournalError::from)?)
    }
    fn creation_quarantined(&self, owner: &str, resource: &str) -> Result<bool, CreateFileError> {
        if crate::workspaces::quarantined(&self.connection, owner, resource)
            .map_err(JournalError::from)?
        {
            return Ok(true);
        }
        self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN creation_consents c ON c.environment=o.environment WHERE c.owner=? AND c.resource_identity=? AND o.state IN('dispatching','unknown'))",params![owner,resource],|r|r.get(0)).map_err(|e|JournalError::from(e).into())
    }
    /// Trusted explicit user consent, separate from a readonly workspace and
    /// from per-operation approval. Repeating a revoked request never revives it.
    pub fn authorize_file_creation(
        &mut self,
        owner: &str,
        request_id: &str,
        policy: &str,
        root: &Path,
    ) -> Result<CreateFileGrant, CreateFileError> {
        if ![owner, request_id, policy].iter().all(|s| valid_id(s)) {
            return Err(JournalError::Invalid.into());
        }
        let canonical = root.canonicalize().map_err(WorkspaceError::Io)?;
        let path = canonical
            .to_str()
            .filter(|p| p.len() <= 4096)
            .ok_or(WorkspaceError::Invalid)?;
        let id = format!("creation-{}", uuid::Uuid::new_v4());
        let mut private = vec![self.private_root.clone()];
        private.extend(self.extra_private_roots.clone());
        let probe = WorkspaceGrant::open(owner, &id, policy, &canonical, &private)?;
        if !probe.has_persistent_identity() {
            return Err(WorkspaceError::PersistentIdentityUnavailable.into());
        }
        let digest = request_digest(
            "create-consent-v1",
            &(owner, path, policy, probe.resource_identity()),
        )
        .map_err(|_| JournalError::Invalid)?;
        let prior:Option<(String,String)>=self.connection.query_row("SELECT digest,consent_id FROM creation_request_keys WHERE owner=? AND request_id=?",params![owner,request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        if let Some((old, id)) = prior {
            if old != digest {
                return Err(JournalError::Conflict.into());
            }
            return self.restore_file_creation(owner, &id);
        }
        let keys: i64 = self
            .connection
            .query_row(
                "SELECT count(*) FROM creation_request_keys WHERE owner=?",
                [owner],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if keys >= 8192 {
            return Err(WorkspaceError::Limit.into());
        }
        if self.creation_quarantined(owner, probe.resource_identity())? {
            return Err(CreateFileError::Unknown);
        }
        let existing:Option<String>=self.connection.query_row("SELECT consent_id FROM creation_consents WHERE owner=? AND canonical_path=? AND active=1",params![owner,path],|r|r.get(0)).optional().map_err(JournalError::from)?;
        if let Some(existing) = existing {
            let view = self.creation_consent(owner, &existing)?;
            if view.policy != policy || view.resource_identity != probe.resource_identity() {
                return Err(JournalError::Conflict.into());
            }
            // Validate current physical identity and local revocation before
            // recording an idempotent alias for the same active consent.
            let grant = self.restore_file_creation(owner, &existing)?;
            self.connection.execute("INSERT INTO creation_request_keys(owner,request_id,digest,consent_id) VALUES(?,?,?,?)",params![owner,request_id,digest,existing]).map_err(JournalError::from)?;
            return Ok(grant);
        }
        let environment = request_digest("create-only-workspace-v1", &probe.environment())
            .map_err(|_| JournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let (total, active): (i64, i64) = tx
            .query_row(
                "SELECT count(*),COALESCE(sum(active),0) FROM creation_consents WHERE owner=?",
                [owner],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(JournalError::from)?;
        if total >= 1000 || active >= 32 {
            return Err(WorkspaceError::Limit.into());
        }
        tx.execute("INSERT INTO creation_consents(consent_id,owner,canonical_path,resource_identity,environment,policy,active,version) VALUES(?,?,?,?,?,?,1,1)",params![id,owner,path,probe.resource_identity(),environment,policy]).map_err(JournalError::from)?;
        tx.execute(
            "INSERT INTO creation_request_keys(owner,request_id,digest,consent_id) VALUES(?,?,?,?)",
            params![owner, request_id, digest, id],
        )
        .map_err(JournalError::from)?;
        tx.execute("INSERT INTO creation_consent_audit(consent_id,owner,kind,version) VALUES(?,?,'creation.authorized',1)",params![id,owner]).map_err(JournalError::from)?;
        tx.commit().map_err(JournalError::from)?;
        self.restore_file_creation(owner, &id)
    }
    /// Called only after the trusted Host reacquires required OS access. Merely
    /// opening a Journal does not recreate a writable directory capability.
    pub fn restore_file_creation(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<CreateFileGrant, CreateFileError> {
        let view = self.creation_consent(owner, id)?;
        if !view.active {
            return Err(WorkspaceError::Revoked.into());
        }
        if self.creation_quarantined(owner, &view.resource_identity)? {
            return Err(CreateFileError::Unknown);
        }
        let mut private = vec![self.private_root.clone()];
        private.extend(self.extra_private_roots.clone());
        let flag = self
            .workspace_signals
            .entry(id.into())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        if flag.load(Ordering::Acquire) {
            return Err(WorkspaceError::Revoked.into());
        }
        let grant = WorkspaceGrant::open_scoped(
            owner,
            id,
            &view.policy,
            Path::new(&view.canonical_path),
            &private,
            Some(flag),
        )?;
        let environment = request_digest("create-only-workspace-v1", &grant.environment())
            .map_err(|_| JournalError::Invalid)?;
        if !grant.has_persistent_identity()
            || grant.resource_identity() != view.resource_identity
            || environment != view.environment
        {
            self.revoke_file_creation(owner, id, &view.version)?;
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let epoch = self
            .connection
            .query_row(
                "SELECT epoch FROM runtime_host WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        Ok(CreateFileGrant {
            grant,
            environment,
            consent_id: id.into(),
            version: view.version.parse().map_err(|_| JournalError::Invalid)?,
            epoch,
        })
    }
    pub fn creation_revoker(
        &mut self,
        owner: &str,
        id: &str,
    ) -> Result<CreationRevoker, CreateFileError> {
        let view = self.creation_consent(owner, id)?;
        if !view.active {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(CreationRevoker {
            flag: self
                .workspace_signals
                .entry(id.into())
                .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                .clone(),
        })
    }
    pub fn revoke_file_creation(
        &mut self,
        owner: &str,
        id: &str,
        expected_version: &str,
    ) -> Result<CreationConsentView, CreateFileError> {
        let expected = expected_version
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0 && n.to_string() == expected_version)
            .ok_or(JournalError::Invalid)?;
        let current = self.creation_consent(owner, id)?;
        let version = current
            .version
            .parse::<i64>()
            .map_err(|_| JournalError::Invalid)?;
        if !current.active {
            if expected == version || expected.checked_add(1) == Some(version) {
                return Ok(current);
            }
            return Err(JournalError::Conflict.into());
        }
        if expected != version {
            return Err(JournalError::Conflict.into());
        }
        if let Some(flag) = self.workspace_signals.get(id) {
            flag.store(true, Ordering::Release);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        if tx.execute("UPDATE creation_consents SET active=0,version=version+1 WHERE consent_id=? AND owner=? AND active=1 AND version=?",params![id,owner,expected]).map_err(JournalError::from)?!=1 {return Err(JournalError::Conflict.into());}
        tx.execute("INSERT INTO creation_consent_audit(consent_id,owner,kind,version) VALUES(?,?,'creation.revoked',?)",params![id,owner,version+1]).map_err(JournalError::from)?;
        tx.commit().map_err(JournalError::from)?;
        self.creation_consent(owner, id)
    }
    pub(crate) fn check_creation_permission(
        &self,
        grant: &CreateFileGrant,
    ) -> Result<(), CreateFileError> {
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM creation_consents c JOIN runtime_host h ON h.singleton=1 WHERE c.consent_id=? AND c.owner=? AND c.environment=? AND c.resource_identity=? AND c.policy=? AND c.active=1 AND c.version=? AND h.epoch=?)",params![grant.consent_id,grant.grant.owner(),grant.environment,grant.grant.resource_identity(),grant.grant.policy(),grant.version,grant.epoch],|r|r.get(0)).map_err(JournalError::from)?;
        if !valid || grant.grant.is_revoked() {
            return Err(WorkspaceError::Revoked.into());
        }
        Ok(())
    }
    pub(crate) fn validate_creation_consent(
        &self,
        grant: &CreateFileGrant,
    ) -> Result<(), CreateFileError> {
        self.check_creation_permission(grant)?;
        if self.creation_quarantined(grant.grant.owner(), grant.grant.resource_identity())? {
            return Err(CreateFileError::Unknown);
        }
        Ok(())
    }
}
