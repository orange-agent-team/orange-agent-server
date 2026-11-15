//! Content-free causal references captured from the immutable model input.
use super::*;
use crate::memory::MemoryReadReceipt;
use std::collections::{BTreeSet, VecDeque};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemorySource {
    Workspace {
        owner: String,
        run: String,
        operation: String,
        resource: String,
        result_digest: String,
    },
    Memory {
        scope: String,
        entry: String,
        version: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryProvenance {
    pub operation_id: String,
    pub invocation_digest: String,
    pub sources: Vec<MemorySource>,
}
impl MemoryProvenance {
    pub fn digest(&self) -> Result<String, EngineError> {
        digest("memory-provenance.v1", self)
    }
}
#[derive(Default)]
pub(in crate::engine) struct SourceProof {
    pub(in crate::engine) files: Vec<workspace::ToolReceipt>,
    memories: Vec<MemoryReadReceipt>,
    roots: Vec<(String, WorkspaceGrant)>,
}
impl SourceProof {
    pub(in crate::engine) fn check(&self, j: &Journal) -> Result<(), EngineError> {
        self.check_db(&j.connection, &j.memory_session, None)
    }
    pub(super) fn check_after(
        &self,
        j: &Journal,
        changed: Option<(&str, &str, i64)>,
    ) -> Result<(), EngineError> {
        self.check_db(&j.connection, &j.memory_session, changed)
    }
    pub(super) fn check_db(
        &self,
        db: &rusqlite::Connection,
        session: &str,
        changed: Option<(&str, &str, i64)>,
    ) -> Result<(), EngineError> {
        if self.files.iter().any(|v| !v.is_authorized()) {
            return Err(EngineError::Forbidden);
        }
        for receipt in &self.memories {
            receipt
                .check_connection(db, session, changed)
                .map_err(error)?;
        }
        for (run, grant) in &self.roots {
            check_provenance_root_db(db, run, grant)?;
        }
        Ok(())
    }
    pub(in crate::engine) fn append(&mut self, mut other: Self) {
        self.files.append(&mut other.files);
        self.memories.append(&mut other.memories);
        self.roots.append(&mut other.roots);
    }
}
type Key = (String, String, String);
impl Journal {
    fn capture_memory_provenance(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<MemoryProvenance, EngineError> {
        self.intent_contract(c, op)?;
        let(raw,sha):(String,String)=self.connection.query_row("SELECT request,request_digest FROM runtime_invocations WHERE invocation=? AND run_id=? AND state='committed'",params![op.invocation,c.run_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if raw.len() > 2 * 1024 * 1024 {
            return Err(EngineError::Invalid);
        }
        let request: GenerateRequest =
            serde_json::from_str(&raw).map_err(|_| EngineError::Invalid)?;
        if digest("model-request-v1", &request)? != sha {
            return Err(EngineError::RecoveryRequired);
        }
        let mut sources = BTreeSet::new();
        for message in request.messages.iter().filter(|m| m.role == Role::Tool) {
            let call = message
                .tool_call_id
                .as_deref()
                .ok_or(EngineError::Invalid)?;
            let id:Option<String>=self.connection.query_row("SELECT operation_id FROM operations WHERE run_id=? AND call_id=? AND owner=? AND context_applied=1",params![c.run_id,call,c.owner],|r|r.get(0)).optional()?;
            let Some(id) = id else {
                let answers:Option<String>=self.connection.query_row("SELECT answers FROM runtime_questions WHERE run_id=? AND call_id=? AND state='answered'",params![c.run_id,call],|r|r.get(0)).optional()?;
                let answers: Vec<String> =
                    serde_json::from_str(&answers.ok_or(EngineError::RecoveryRequired)?)
                        .map_err(|_| EngineError::Invalid)?;
                if encode(&serde_json::json!({"answers":answers}))? != message.content {
                    return Err(EngineError::RecoveryRequired);
                }
                continue;
            };
            let source = self.operation_record(c, &id)?;
            self.intent_contract(c, &source)?;
            self.checked_cached_result(c, &source)?;
            if source.model_result.as_deref() != Some(&message.content) {
                return Err(EngineError::RecoveryRequired);
            }
            if source.state == "failed" {
                continue;
            }
            if source.state != "succeeded" {
                return Err(EngineError::RecoveryRequired);
            }
            match &source.request {
                AgentRequest::Workspace(
                    WorkspaceRequest::ReadFile(_)
                    | WorkspaceRequest::ListDirectory(_)
                    | WorkspaceRequest::SearchFiles(_)
                    | WorkspaceRequest::SearchText(_),
                ) => {
                    let resource:Option<String>=self.connection.query_row("SELECT resource FROM runtime_workspace_roots WHERE run_id=? AND owner=? AND environment=? AND policy=?",params![c.run_id,c.owner,c.environment,c.policy],|r|r.get(0)).optional()?;
                    let result_digest: String = self.connection.query_row(
                        "SELECT result_digest FROM operations WHERE operation_id=?",
                        [&id],
                        |r| r.get(0),
                    )?;
                    sources.insert(MemorySource::Workspace {
                        owner: c.owner.clone(),
                        run: c.run_id.clone(),
                        operation: id,
                        resource: resource.ok_or(EngineError::ExecutionEnvironmentUnavailable)?,
                        result_digest,
                    });
                }
                AgentRequest::Memory(request) => {
                    let row:Option<(String,String,i64)>=self.connection.query_row("SELECT scope_id,entry_id,entry_version FROM runtime_memory_sources WHERE operation_id=?",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                    if let Some((scope, entry, version)) = row {
                        if scope != request.scope() {
                            return Err(EngineError::RecoveryRequired);
                        }
                        sources.insert(MemorySource::Memory {
                            scope,
                            entry,
                            version: version.to_string(),
                        });
                    } else if !matches!(request,MemoryRequest::MemoryWrite(v) if v.mutation==crate::memory::MemoryMutation::Delete)
                    {
                        return Err(EngineError::RecoveryRequired);
                    }
                }
                _ => return Err(EngineError::ExecutionEnvironmentUnavailable),
            }
            if sources.len() > 128 {
                return Err(EngineError::Invalid);
            }
        }
        let value = MemoryProvenance {
            operation_id: op.id.clone(),
            invocation_digest: sha,
            sources: sources.into_iter().collect(),
        };
        if encode(&value)?.len() > 64 * 1024 {
            return Err(EngineError::Invalid);
        }
        Ok(value)
    }
    pub(super) fn memory_provenance(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<MemoryProvenance, EngineError> {
        let captured = self.capture_memory_provenance(c, op)?;
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT body,digest FROM runtime_memory_lineage WHERE operation_id=?",
                [&op.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((body, sha)) = row {
            if body.len() > 64 * 1024 {
                return Err(EngineError::Invalid);
            }
            let value: MemoryProvenance =
                serde_json::from_str(&body).map_err(|_| EngineError::Invalid)?;
            if value.digest()? != sha || value != captured {
                return Err(EngineError::RecoveryRequired);
            }
            Ok(value)
        } else {
            // Never invent a historical physical root. Old memory-only snapshots can
            // be independently reconstructed; workspace sources require a saved root.
            Ok(captured)
        }
    }
    pub(super) fn bind_memory_provenance(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<MemoryProvenance, EngineError> {
        let value = self.memory_provenance(c, op)?;
        self.connection.execute(
            "INSERT OR IGNORE INTO runtime_memory_lineage(operation_id,body,digest) VALUES(?,?,?)",
            params![op.id, encode(&value)?, value.digest()?],
        )?;
        Ok(value)
    }
    pub(super) fn memory_preview_digest(
        &self,
        op: &StoredOperation,
        review: &MemoryReview,
        provenance: &MemoryProvenance,
    ) -> Result<String, EngineError> {
        digest(
            "agent-memory-approval.v1",
            &(
                &op.id,
                &op.digest,
                &op.environment,
                &review.digest,
                provenance.digest()?,
            ),
        )
    }
    fn check_provenance_root(&self, run: &str, grant: &WorkspaceGrant) -> Result<(), EngineError> {
        check_provenance_root_db(&self.connection, run, grant)
    }
    fn fact_provenance(
        &self,
        scope: &str,
        entry: &str,
        version: &str,
    ) -> Result<MemoryProvenance, EngineError> {
        type Row = (String, String, String, String, Option<String>);
        let mut statement=self.connection.prepare("SELECT d.draft_id,d.actor,d.body,d.digest,m.operation_id FROM memory_drafts d LEFT JOIN runtime_memory_operations m ON m.draft_id=d.draft_id WHERE d.scope_id=? AND d.entry_id=? AND d.result_version=? AND d.state='approved' AND json_extract(d.body,'$.mutation')='put'")?;
        let rows = statement
            .query_map(params![scope, entry, version], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<Row>, _>>()?;
        let [(id, actor, body, sha, operation)] = rows.as_slice() else {
            return Err(EngineError::RecoveryRequired);
        };
        if body.len() > 8192 {
            return Err(EngineError::Invalid);
        }
        let record: serde_json::Value =
            serde_json::from_str(body).map_err(|_| EngineError::Invalid)?;
        let outcome = self
            .owned_memory_draft_metadata(actor, scope, id)
            .map_err(error)?
            .ok_or(EngineError::RecoveryRequired)?;
        if &outcome.digest != sha || outcome.result_version.as_deref() != Some(version) {
            return Err(EngineError::RecoveryRequired);
        }
        if record["origin"] == "user_declaration" {
            return Ok(MemoryProvenance {
                operation_id: String::new(),
                invocation_digest: String::new(),
                sources: vec![],
            });
        }
        let source_run = record["source_run"].as_str().ok_or(EngineError::Invalid)?;
        if let Some(operation) = operation {
            let config = self.stored_run(actor, source_run)?.config;
            let op = self.operation_record(&config, operation)?;
            self.memory_provenance(&config, &op)
        } else {
            let any: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE run_id=?)",
                [source_run],
                |r| r.get(0),
            )?;
            if any {
                return Err(EngineError::ExecutionEnvironmentUnavailable);
            }
            Ok(MemoryProvenance {
                operation_id: String::new(),
                invocation_digest: String::new(),
                sources: vec![],
            })
        }
    }
    pub(super) async fn verify_memory_provenance(
        &self,
        c: &RunConfig,
        authority: AgentAuthority<'_>,
        sources: Vec<MemorySource>,
        root: Option<Key>,
    ) -> Result<SourceProof, EngineError> {
        if authority.source_workspaces.len() > 16 || authority.source_memories.len() > 16 {
            return Err(EngineError::Invalid);
        }
        let mut proof = SourceProof::default();
        let mut queue = VecDeque::new();
        for value in sources {
            queue.push_back((value, 0usize, Vec::<Key>::new()));
        }
        let mut visited = BTreeSet::new();
        let mut count = 0;
        while let Some((source, depth, ancestors)) = queue.pop_front() {
            count += 1;
            if count > 256 || depth > 16 {
                return Err(EngineError::Invalid);
            }
            match source {
                MemorySource::Workspace {
                    owner,
                    run,
                    operation,
                    resource,
                    result_digest,
                } => {
                    let grant = std::iter::once(authority.read)
                        .chain(authority.source_workspaces.iter())
                        .find(|g| {
                            g.owner() == c.owner
                                && g.policy() == c.policy
                                && g.resource_identity() == resource
                                && !g.is_revoked()
                        })
                        .ok_or(EngineError::Forbidden)?;
                    self.check_provenance_root(&run, grant)?;
                    let config = self.stored_run(&owner, &run)?.config;
                    let op = self.operation_record(&config, &operation)?;
                    self.intent_contract(&config, &op)?;
                    self.checked_cached_result(&config, &op)?;
                    let expected:String=self.connection.query_row("SELECT result_digest FROM operations WHERE operation_id=? AND state='succeeded'",[&operation],|r|r.get(0))?;
                    if expected != result_digest {
                        return Err(EngineError::RecoveryRequired);
                    }
                    let request = op.request.workspace()?.clone();
                    if !matches!(
                        request,
                        WorkspaceRequest::ReadFile(_)
                            | WorkspaceRequest::ListDirectory(_)
                            | WorkspaceRequest::SearchFiles(_)
                            | WorkspaceRequest::SearchText(_)
                    ) {
                        return Err(EngineError::ExecutionEnvironmentUnavailable);
                    }
                    let receipt = grant
                        .execute(&c.owner, grant.environment(), &c.policy, request)
                        .await?;
                    if digest("tool-result-v1", receipt.result())? != result_digest {
                        return Err(EngineError::RecoveryRequired);
                    }
                    proof.files.push(receipt);
                    proof.roots.push((run, grant.clone()));
                }
                MemorySource::Memory {
                    scope,
                    entry,
                    version,
                } => {
                    let key = (scope.clone(), entry.clone(), version.clone());
                    if ancestors.contains(&key) {
                        return Err(EngineError::RecoveryRequired);
                    }
                    if !visited.insert(key.clone()) {
                        continue;
                    }
                    let grant = authority
                        .memories
                        .iter()
                        .chain(authority.source_memories.iter())
                        .find(|g| g.scope_id() == scope && !g.is_revoked())
                        .ok_or(EngineError::Forbidden)?;
                    self.memory_capability(grant, &c.owner, &c.policy)
                        .map_err(error)?;
                    let historical = root.as_ref().is_some_and(|(s, e, v)| {
                        s == &scope
                            && e == &entry
                            && version
                                .parse::<i64>()
                                .ok()
                                .zip(v.parse::<i64>().ok())
                                .is_some_and(|(old, new)| old < new)
                    });
                    if !historical {
                        proof.memories.push(
                            self.read_memory_for_agent(
                                grant,
                                &entry,
                                Some(&version),
                                0,
                                4096,
                                false,
                            )
                            .map_err(error)?,
                        );
                    }
                    let graph = self.fact_provenance(&scope, &entry, &version)?;
                    let mut path = ancestors;
                    path.push(key);
                    for child in graph.sources {
                        queue.push_back((child, depth + 1, path.clone()));
                    }
                }
            }
        }
        proof.check(self)?;
        Ok(proof)
    }
    pub(super) async fn verify_memory_fact(
        &self,
        c: &RunConfig,
        authority: AgentAuthority<'_>,
        scope: &str,
        entry: &str,
        version: &str,
    ) -> Result<SourceProof, EngineError> {
        let root = (scope.into(), entry.into(), version.into());
        self.verify_memory_provenance(
            c,
            authority,
            vec![MemorySource::Memory {
                scope: scope.into(),
                entry: entry.into(),
                version: version.into(),
            }],
            Some(root),
        )
        .await
    }
    pub(in crate::engine) async fn verify_run_memory_provenance(
        &self,
        c: &RunConfig,
        authority: AgentAuthority<'_>,
    ) -> Result<SourceProof, EngineError> {
        let rows = {
            let mut q=self.connection.prepare("SELECT DISTINCT m.scope_id,m.entry_id,m.entry_version FROM runtime_memory_sources m JOIN operations o USING(operation_id) WHERE o.run_id=? AND o.state='succeeded'")?;
            q.query_map([&c.run_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut proof = SourceProof::default();
        for (scope, entry, version) in rows {
            proof.append(
                self.verify_memory_fact(c, authority, &scope, &entry, &version.to_string())
                    .await?,
            );
        }
        proof.check(self)?;
        Ok(proof)
    }
}

fn check_provenance_root_db(
    db: &rusqlite::Connection,
    run: &str,
    grant: &WorkspaceGrant,
) -> Result<(), EngineError> {
    let(resource,owner,consent,version):(String,String,Option<String>,Option<i64>)=db.query_row("SELECT resource,owner,workspace_id,consent_version FROM runtime_workspace_roots WHERE run_id=?",[run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if !grant.has_persistent_identity()
        || grant.is_revoked()
        || grant.resource_identity() != resource
    {
        return Err(EngineError::Forbidden);
    }
    if let Some(id) = consent {
        let live:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM local_workspaces WHERE workspace_id=? AND owner=? AND resource_identity=? AND active=1 AND version=?)",params![id,owner,resource,version],|r|r.get(0))?;
        if !live {
            return Err(EngineError::Forbidden);
        }
    }
    let active: Option<bool> = db
        .query_row(
            "SELECT active FROM local_workspaces WHERE owner=? AND environment=?",
            params![grant.owner(), grant.environment()],
            |r| r.get(0),
        )
        .optional()?;
    if active == Some(false) {
        return Err(EngineError::Forbidden);
    }
    Ok(())
}
