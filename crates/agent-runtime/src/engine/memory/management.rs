//! User management reads do not manufacture a workspace or an Agent Run.
use super::*;
use crate::memory::{MemoryReadReceipt, MemorySummary, MemoryValue};
use lineage::{SourceAccess, SourceProof};
#[derive(Clone, Copy)]
pub struct MemoryManagementAuthority<'a> {
    grant: &'a MemoryGrant,
    workspaces: &'a [WorkspaceGrant],
    memories: &'a [MemoryGrant],
}
impl<'a> MemoryManagementAuthority<'a> {
    pub fn new(grant: &'a MemoryGrant) -> Self {
        Self {
            grant,
            workspaces: &[],
            memories: &[],
        }
    }
    pub fn with_source_workspaces(mut self, grants: &'a [WorkspaceGrant]) -> Self {
        self.workspaces = grants;
        self
    }
    pub fn with_source_memories(mut self, grants: &'a [MemoryGrant]) -> Self {
        self.memories = grants;
        self
    }
    fn sources(&self, disabled_root: bool) -> SourceAccess<'a> {
        SourceAccess {
            actor: self.grant.actor(),
            policy: self.grant.policy(),
            workspaces: self.workspaces.iter().collect(),
            memories: std::iter::once(self.grant)
                .chain(self.memories.iter())
                .collect(),
            disabled_root,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryReadSelection {
    pub entry_id: String,
    pub expected_version: Option<String>,
    pub start_byte: u32,
    pub max_bytes: u32,
    /// Management only; model tools must continue requesting active records.
    pub include_disabled: bool,
}
pub struct VerifiedMemoryRead {
    receipt: MemoryReadReceipt,
    proof: SourceProof,
}
impl VerifiedMemoryRead {
    pub fn view<'a>(&'a self, journal: &'a Journal) -> Result<&'a MemoryValue, EngineError> {
        self.proof.check(journal)?;
        self.receipt.view(journal).map_err(error)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedMemoryPage {
    pub scope_id: String,
    pub entries: Vec<MemorySummary>,
    pub next_cursor: Option<String>,
    /// No IDs, titles, contents or counts of filtered records are returned.
    pub omitted_unavailable: bool,
    pub scan_limited: bool,
}
pub struct VerifiedMemoryPage {
    page: ManagedMemoryPage,
    reads: Vec<VerifiedMemoryRead>,
    grant: MemoryGrant,
    lease: crate::memory::MemoryLease,
}
impl VerifiedMemoryPage {
    pub fn view<'a>(&'a self, journal: &'a Journal) -> Result<&'a ManagedMemoryPage, EngineError> {
        self.lease
            .check_db(&journal.connection, &journal.memory_session)
            .map_err(error)?;
        journal
            .memory_capability(&self.grant, self.grant.actor(), self.grant.policy())
            .map_err(error)?;
        for read in &self.reads {
            read.view(journal)?;
        }
        Ok(&self.page)
    }
}
impl Journal {
    pub async fn read_memory_verified(
        &self,
        authority: MemoryManagementAuthority<'_>,
        selection: MemoryReadSelection,
    ) -> Result<VerifiedMemoryRead, EngineError> {
        if authority.workspaces.len() > 17 || authority.memories.len() > 23 {
            return Err(EngineError::Invalid);
        }
        let receipt = self
            .read_memory_for_agent(
                authority.grant,
                &selection.entry_id,
                selection.expected_version.as_deref(),
                selection.start_byte,
                selection.max_bytes,
                selection.include_disabled,
            )
            .map_err(error)?;
        let value = receipt.view(self).map_err(error)?;
        let root = (
            value.scope_id.clone(),
            value.entry_id.clone(),
            value.version.clone(),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let proof = tokio::time::timeout_at(
            deadline,
            self.verify_memory_provenance_scoped(
                authority.sources(selection.include_disabled),
                vec![MemorySource::Memory {
                    scope: root.0.clone(),
                    entry: root.1.clone(),
                    version: root.2.clone(),
                }],
                Some(root),
            ),
        )
        .await
        .map_err(|_| EngineError::RecoveryRequired)??;
        if tokio::time::Instant::now() >= deadline {
            return Err(EngineError::RecoveryRequired);
        }
        let result = VerifiedMemoryRead { receipt, proof };
        result.view(self)?;
        Ok(result)
    }
    pub async fn list_memory_verified(
        &self,
        authority: MemoryManagementAuthority<'_>,
        cursor: Option<&str>,
        limit: u16,
        include_disabled: bool,
    ) -> Result<VerifiedMemoryPage, EngineError> {
        if !(1..=100).contains(&limit)
            || cursor.is_some_and(|v| !valid_id(v))
            || authority.workspaces.len() > 17
            || authority.memories.len() > 23
        {
            return Err(EngineError::Invalid);
        }
        self.memory_capability(
            authority.grant,
            authority.grant.actor(),
            authority.grant.policy(),
        )
        .map_err(error)?;
        let capability = self
            .memory_capability(
                authority.grant,
                authority.grant.actor(),
                authority.grant.policy(),
            )
            .map_err(error)?;
        let after = if let Some(token) = cursor {
            let state = self
                .memory_cursors
                .borrow()
                .get(token)
                .cloned()
                .ok_or(EngineError::Invalid)?;
            if state.actor != authority.grant.actor()
                || state.scope != authority.grant.scope_id()
                || state.environment != capability.environment
                || state.include_disabled != include_disabled
                || state.expires <= std::time::Instant::now()
            {
                return Err(EngineError::Forbidden);
            }
            Some(state.after)
        } else {
            None
        };
        let lease = authority.grant.lease().map_err(error)?;
        // Scan only IDs/versions first. No title is projected until its proof passes.
        let rows = {
            let mut q=self.connection.prepare("SELECT entry_id,version FROM memory_entries WHERE scope_id=?1 AND (?2 IS NULL OR entry_id>?2) AND (state='active' OR (?3=1 AND state='disabled')) ORDER BY entry_id LIMIT 201")?;
            q.query_map(
                params![authority.grant.scope_id(), after, include_disabled],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut reads = Vec::new();
        let mut entries = Vec::new();
        let mut omitted = false;
        let mut scanned = 0;
        let mut cursor = None;
        for (id, version) in rows.iter().take(200) {
            if tokio::time::Instant::now() >= deadline {
                return Err(EngineError::RecoveryRequired);
            }
            if entries.len() >= usize::from(limit) {
                break;
            }
            scanned += 1;
            cursor = Some(id.clone());
            let read = tokio::time::timeout_at(
                deadline,
                self.read_memory_verified(
                    authority,
                    MemoryReadSelection {
                        entry_id: id.clone(),
                        expected_version: Some(version.to_string()),
                        start_byte: 0,
                        max_bytes: 4096,
                        include_disabled,
                    },
                ),
            )
            .await
            .map_err(|_| EngineError::RecoveryRequired)?;
            if tokio::time::Instant::now() >= deadline {
                return Err(EngineError::RecoveryRequired);
            }
            match read {
                Ok(read) => {
                    let value = read.view(self)?;
                    entries.push(MemorySummary {
                        entry_id: value.entry_id.clone(),
                        version: value.version.clone(),
                        title: value.title.clone(),
                        enabled: value.enabled,
                        origin: value.origin,
                        content_bytes: value.total_bytes.clone(),
                    });
                    reads.push(read);
                }
                Err(
                    EngineError::Forbidden
                    | EngineError::RecoveryRequired
                    | EngineError::ExecutionEnvironmentUnavailable
                    | EngineError::Workspace(_),
                ) => {
                    self.memory_capability(
                        authority.grant,
                        authority.grant.actor(),
                        authority.grant.policy(),
                    )
                    .map_err(error)?;
                    omitted = true;
                }
                Err(e) => return Err(e),
            }
        }
        let more = scanned < rows.len();
        let next_cursor = if more {
            let mut cursors = self.memory_cursors.borrow_mut();
            cursors.retain(|_, v| v.expires > std::time::Instant::now());
            if cursors.len() >= 256 {
                return Err(EngineError::Invalid);
            }
            let token = uuid::Uuid::new_v4().to_string();
            cursors.insert(
                token.clone(),
                crate::memory::ManagementCursor {
                    actor: authority.grant.actor().into(),
                    scope: authority.grant.scope_id().into(),
                    environment: capability.environment,
                    after: cursor.ok_or(EngineError::Invalid)?,
                    include_disabled,
                    expires: std::time::Instant::now() + Duration::from_secs(300),
                },
            );
            Some(token)
        } else {
            None
        };
        let page = ManagedMemoryPage {
            scope_id: authority.grant.scope_id().into(),
            entries,
            next_cursor,
            omitted_unavailable: omitted,
            scan_limited: scanned == 200 && rows.len() > 200,
        };
        let result = VerifiedMemoryPage {
            page,
            reads,
            grant: authority.grant.clone(),
            lease,
        };
        result.view(self)?;
        Ok(result)
    }
}
