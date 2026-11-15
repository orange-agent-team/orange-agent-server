mod discovery;
mod management;
pub use management::{
    ManagedMemoryPage, MemoryManagementAuthority, MemoryReadSelection, VerifiedMemoryPage,
    VerifiedMemoryRead,
};
mod lineage;
use super::*;
use crate::memory::{
    MemoryCapability, MemoryDraftOutcome, MemoryError, MemoryGrant, MemoryOrigin, MemoryReview,
    MemoryWriteRequest, tools::MemoryRequest,
};
pub(in crate::engine) use lineage::SourceProof;
pub use lineage::{MemoryProvenance, MemorySource};
pub struct AgentMemoryApproval<'a> {
    pub operation_id: &'a str,
    pub preview_digest: &'a str,
    pub expires_at: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct AgentMemoryPreview {
    pub operation_id: String,
    pub argument_digest: String,
    pub review: MemoryReview,
    pub approval_digest: String,
    pub provenance: MemoryProvenance,
}
pub(super) fn error(e: MemoryError) -> EngineError {
    match e {
        MemoryError::Denied => EngineError::Forbidden,
        MemoryError::ApprovalRequired => EngineError::ApprovalRequired,
        MemoryError::Conflict => EngineError::RecoveryRequired,
        MemoryError::SourceRequired => EngineError::ExecutionEnvironmentUnavailable,
        MemoryError::Storage(e) => EngineError::Storage(e),
        MemoryError::Journal(e) => EngineError::Journal(e),
        _ => EngineError::Invalid,
    }
}
pub(in crate::engine) async fn wait_authority_revoked(authority: AgentAuthority<'_>) {
    tokio::select! {
        _=wait_revoked(authority.memories)=>{},
        _=wait_revoked(authority.source_memories)=>{},
        _=async {loop {if authority.read.is_revoked()||authority.source_workspaces.iter().any(|g|g.is_revoked()){break;}tokio::time::sleep(Duration::from_millis(25)).await;}}=>{},
    }
}
pub(super) async fn wait_revoked(grants: &[MemoryGrant]) {
    if grants.is_empty() {
        std::future::pending::<()>().await;
        return;
    }
    let mut waits = tokio::task::JoinSet::new();
    for grant in grants {
        let grant = grant.clone();
        waits.spawn(async move {
            grant.revoked().await;
        });
    }
    waits.join_next().await;
}
impl Journal {
    pub fn agent_memory_requirements(
        &self,
        owner: &str,
        run: &str,
    ) -> Result<Vec<MemoryCapability>, EngineError> {
        Ok(self.stored_run(owner, run)?.memories)
    }
    pub(super) fn memory_requirements(
        &self,
        c: &RunConfig,
        grants: &[MemoryGrant],
    ) -> Result<Vec<MemoryCapability>, EngineError> {
        if grants.len() > 8 {
            return Err(EngineError::Invalid);
        }
        let mut result = grants
            .iter()
            .map(|g| {
                self.memory_capability(g, &c.owner, &c.policy)
                    .map_err(error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        result.sort_by(|a, b| a.scope_id.cmp(&b.scope_id));
        if result.windows(2).any(|v| v[0].scope_id == v[1].scope_id) {
            return Err(EngineError::Invalid);
        }
        Ok(result)
    }
    pub(super) fn memory_bindings(&self, run: &str) -> Result<Vec<MemoryCapability>, EngineError> {
        let mut statement=self.connection.prepare("SELECT scope_id,scope_revision,can_write,environment FROM runtime_memory_capabilities WHERE run_id=? ORDER BY scope_id")?;
        let values = statement
            .query_map([run], |r| {
                Ok(MemoryCapability {
                    scope_id: r.get(0)?,
                    revision: r.get::<_, i64>(1)?.to_string(),
                    write: r.get(2)?,
                    environment: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if values.len() > 8 {
            return Err(EngineError::Invalid);
        }
        Ok(values)
    }
    pub(super) fn check_memory_bindings(
        &self,
        stored: &StoredRun,
        grants: &[MemoryGrant],
    ) -> Result<(), EngineError> {
        if self.memory_requirements(&stored.config, grants)? != stored.memories {
            return Err(EngineError::Forbidden);
        }
        Ok(())
    }
    fn memory_grant_for<'a>(
        &self,
        stored: &StoredRun,
        op: &StoredOperation,
        grants: &'a [MemoryGrant],
    ) -> Result<&'a MemoryGrant, EngineError> {
        self.check_memory_bindings(stored, grants)?;
        let AgentRequest::Memory(request) = &op.request else {
            return Err(EngineError::Invalid);
        };
        let binding = stored
            .memories
            .iter()
            .find(|v| v.scope_id == request.scope())
            .ok_or(EngineError::Forbidden)?;
        if binding.environment != op.environment
            || matches!(request, MemoryRequest::MemoryWrite(_)) && !binding.write
        {
            return Err(EngineError::Forbidden);
        }
        grants
            .iter()
            .find(|v| v.scope_id() == request.scope())
            .ok_or(EngineError::Forbidden)
    }
    pub fn start_agent_authorized(
        &mut self,
        c: &RunConfig,
        messages: Vec<Message>,
        authority: AgentAuthority<'_>,
        user_questions: bool,
    ) -> Result<RunView, EngineError> {
        self.start_agent_mode(c, messages, authority, user_questions)
    }
    /// Host opt-in; persisted per Run. Existing Run registrations remain v1.
    pub fn start_agent_discovering(
        &mut self,
        c: &RunConfig,
        messages: Vec<Message>,
        authority: AgentAuthority<'_>,
        user_questions: bool,
    ) -> Result<RunView, EngineError> {
        self.start_agent_protocol(c, messages, authority, user_questions, 2)
    }
    pub fn agent_memory_read_version(&self, owner: &str, run: &str) -> Result<u8, EngineError> {
        Ok(self.stored_run(owner, run)?.memory_read_version)
    }
    pub async fn advance_agent_authorized<G: ModelGateway>(
        &mut self,
        owner: &str,
        run: &str,
        authority: AgentAuthority<'_>,
        gateway: &G,
        cancel: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        self.advance_agent_mode(owner, run, authority, gateway, cancel)
            .await?;
        self.agent_view_authorized_scopes(owner, run, authority)
            .await
    }
    pub async fn agent_memory_preview(
        &mut self,
        owner: &str,
        run: &str,
        operation: &str,
        authority: AgentAuthority<'_>,
    ) -> Result<AgentMemoryPreview, EngineError> {
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, authority.read)?;
        let op = self
            .pending_operations(run)?
            .into_iter()
            .find(|v| v.id == operation && v.state == "intended")
            .ok_or(EngineError::ApprovalRequired)?;
        let grant = self.memory_grant_for(&stored, &op, authority.memories)?;
        let AgentRequest::Memory(MemoryRequest::MemoryWrite(args)) = &op.request else {
            return Err(EngineError::Invalid);
        };
        self.intent_contract(&stored.config, &op)?;
        let _leases = self
            .revalidate_sources_authority(&stored.config, authority)
            .await?;
        let entry: String = self.connection.query_row(
            "SELECT entry_id FROM runtime_memory_operations WHERE operation_id=? AND scope_id=?",
            params![op.id, args.scope_id],
            |r| r.get(0),
        )?;
        let provenance = self.bind_memory_provenance(&stored.config, &op)?;
        let proof = self
            .verify_operation_memory_sources(&stored.config, authority, provenance.sources.clone())
            .await?;
        let prepared = self
            .prepare_memory_write_for_agent(
                grant,
                &op.id,
                MemoryWriteRequest {
                    entry_id: entry,
                    expected_version: args.expected_version.clone(),
                    source_run: run.into(),
                    origin: MemoryOrigin::ModelInference,
                    mutation: args.mutation,
                    payload: args.payload.clone(),
                },
            )
            .map_err(error)?;
        let review = prepared.view(self).map_err(error)?.clone();
        prepared.view(self).map_err(error)?;
        proof.check(self)?;
        let approval_digest = self.memory_preview_digest(&op, &review, &provenance)?;
        Ok(AgentMemoryPreview {
            operation_id: op.id,
            argument_digest: op.digest,
            review,
            approval_digest,
            provenance,
        })
    }
    pub async fn approve_agent_memory(
        &mut self,
        owner: &str,
        run: &str,
        approval: AgentMemoryApproval<'_>,
        authority: AgentAuthority<'_>,
    ) -> Result<(), EngineError> {
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, authority.read)?;
        if stored.phase != "tools_pending"
            || approval.expires_at <= now()?
            || approval.expires_at > stored.config.deadline_unix
        {
            return Err(EngineError::ApprovalRequired);
        }
        let op = self
            .pending_operations(run)?
            .into_iter()
            .find(|v| v.id == approval.operation_id && v.state == "intended")
            .ok_or(EngineError::ApprovalRequired)?;
        let grant = self.memory_grant_for(&stored, &op, authority.memories)?;
        let _leases = self
            .revalidate_sources_authority(&stored.config, authority)
            .await?;
        let (_, pin) = self.intent_contract(&stored.config, &op)?;
        let expected = match &op.request {
            AgentRequest::Memory(MemoryRequest::MemoryRead(_) | MemoryRequest::MemoryReadV2(_)) => {
                op.digest.clone()
            }
            AgentRequest::Memory(MemoryRequest::MemoryWrite(_)) => {
                let id: String = self.connection.query_row(
                    "SELECT draft_id FROM runtime_memory_operations WHERE operation_id=?",
                    [&op.id],
                    |r| r.get(0),
                )?;
                let prepared = self
                    .review_memory_draft_for_agent(grant, &id)
                    .map_err(error)?;
                let review = prepared.view(self).map_err(error)?;
                let graph = self.memory_provenance(&stored.config, &op)?;
                self.memory_preview_digest(&op, review, &graph)?
            }
            _ => return Err(EngineError::Forbidden),
        };
        if expected != approval.preview_digest {
            return Err(EngineError::ApprovalRequired);
        }
        let source_proof = if matches!(
            op.request,
            AgentRequest::Memory(MemoryRequest::MemoryWrite(_))
        ) {
            let graph = self.memory_provenance(&stored.config, &op)?;
            Some(
                self.verify_operation_memory_sources(&stored.config, authority, graph.sources)
                    .await?,
            )
        } else {
            None
        };
        let core_digest = if matches!(
            op.request,
            AgentRequest::Memory(MemoryRequest::MemoryWrite(_))
        ) {
            let id: String = self.connection.query_row(
                "SELECT draft_id FROM runtime_memory_operations WHERE operation_id=?",
                [&op.id],
                |r| r.get(0),
            )?;
            self.review_memory_draft_for_agent(grant, &id)
                .map_err(error)?
                .view(self)
                .map_err(error)?
                .digest
                .clone()
        } else {
            expected.clone()
        };
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        self.approve(&op.operation(&stored.config), owner, approval.expires_at)?;
        self.record_tool_approval_pin(&op, &pin)?;
        if self.connection.execute("UPDATE runtime_memory_operations SET approval_digest=? WHERE operation_id=? AND (approval_digest IS NULL OR approval_digest=?)",params![core_digest,op.id,core_digest])?!=1{return Err(EngineError::ApprovalRequired);}
        if let Some(proof) = source_proof {
            proof.check(self)?;
            if tx.execute("UPDATE runtime_memory_lineage SET approval_digest=? WHERE operation_id=? AND approval_digest IS NULL",params![expected,op.id])?!=1 {
                let old:String=tx.query_row("SELECT approval_digest FROM runtime_memory_lineage WHERE operation_id=?",[&op.id],|r|r.get(0))?;if old!=expected{return Err(EngineError::ApprovalRequired);}
            }
        }
        self.memory_grant_for(&stored, &op, authority.memories)?;
        tx.commit()?;
        Ok(())
    }
    pub(super) async fn advance_memory_tool(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
        authority: AgentAuthority<'_>,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        let c = &stored.config;
        let grant = self.memory_grant_for(stored, op, authority.memories)?;
        let _leases = self.revalidate_sources_authority(c, authority).await?;
        if *cancel.borrow() || cancel.has_changed().is_err() || grant.is_revoked() {
            return Err(EngineError::Forbidden);
        }
        let approved: String = self.connection.query_row(
            "SELECT approval_digest FROM runtime_memory_operations WHERE operation_id=?",
            [&op.id],
            |r| r.get(0),
        )?;
        let request = match &op.request {
            AgentRequest::Memory(v) => v,
            _ => return Err(EngineError::Invalid),
        };
        if matches!(request, MemoryRequest::MemoryReadV2(_)) {
            if approved != op.digest {
                return Err(EngineError::ApprovalRequired);
            }
            return self
                .advance_memory_discovery(stored, op, authority, cancel)
                .await;
        }
        let mut provenance_proof = SourceProof::default();
        let prepared = if matches!(request, MemoryRequest::MemoryWrite(_)) {
            let draft: String = self.connection.query_row(
                "SELECT draft_id FROM runtime_memory_operations WHERE operation_id=?",
                [&op.id],
                |r| r.get(0),
            )?;
            let value = self
                .review_memory_draft_for_agent(grant, &draft)
                .map_err(error)?;
            if value.view(self).map_err(error)?.digest != approved {
                return Err(EngineError::ApprovalRequired);
            }
            let graph = self.memory_provenance(c, op)?;
            let expected =
                self.memory_preview_digest(op, value.view(self).map_err(error)?, &graph)?;
            let approved_graph: Option<String> = self
                .connection
                .query_row(
                    "SELECT approval_digest FROM runtime_memory_lineage WHERE operation_id=?",
                    [&op.id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if approved_graph.as_deref() != Some(&expected) {
                return Err(EngineError::ApprovalRequired);
            }
            provenance_proof = self
                .verify_operation_memory_sources(c, authority, graph.sources)
                .await?;
            Some(value)
        } else {
            if approved != op.digest {
                return Err(EngineError::ApprovalRequired);
            }
            if let MemoryRequest::MemoryRead(args) = request {
                // Lookup and version/ACL validation happens under the already
                // recorded user read approval, before model-visible content.
                let receipt = self.read_memory_for_agent(
                    grant,
                    &args.entry_id,
                    args.expected_version.as_deref(),
                    args.start_byte.unwrap_or(0),
                    args.max_bytes.unwrap_or(4096),
                    false,
                );
                if let Ok(receipt) = receipt {
                    let v = receipt.view(self).map_err(error)?;
                    provenance_proof = self
                        .verify_memory_fact(
                            c,
                            authority,
                            &args.scope_id,
                            &args.entry_id,
                            &v.version,
                        )
                        .await?;
                }
            }
            None
        };
        provenance_proof.check(self)?;
        let changed = match request {
            MemoryRequest::MemoryWrite(v) => v
                .expected_version
                .as_deref()
                .map(|s| s.parse::<i64>().map_err(|_| EngineError::Invalid))
                .transpose()?
                .and_then(|n| n.checked_add(1))
                .zip(v.entry_id.as_deref())
                .map(|(n, id)| (v.scope_id.as_str(), id, n)),
            _ => None,
        };
        let journal_session = self.memory_session.clone();
        self.claim(&op.operation(c), now()?)?;
        self.connection.execute(
            "UPDATE runtime_runs SET steps=steps+1 WHERE run_id=?",
            [&c.run_id],
        )?;
        let result: Result<(serde_json::Value, Option<(String, String)>), EngineError> =
            match request {
                MemoryRequest::MemoryReadV2(_) => return Err(EngineError::Invalid),
                MemoryRequest::MemoryRead(args) => self
                    .read_memory_for_agent(
                        grant,
                        &args.entry_id,
                        args.expected_version.as_deref(),
                        args.start_byte.unwrap_or(0),
                        args.max_bytes.unwrap_or(4096),
                        false,
                    )
                    .map_err(error)
                    .and_then(|receipt| {
                        let value = receipt.view(self).map_err(error)?.clone();

                        let source = (value.entry_id.clone(), value.version.clone());
                        Ok((
                            serde_json::json!({"kind":"memory_read","data":value}),
                            Some(source),
                        ))
                    }),
                MemoryRequest::MemoryWrite(args) => self
                    .approve_memory_write_checked(prepared.ok_or(EngineError::Invalid)?, &approved,|db|{
                        let time=now().map_err(|_|MemoryError::Invalid)?;
                        provenance_proof.check_db(db,&journal_session,changed).map_err(|_|MemoryError::Denied)?;
                        if time>=c.deadline_unix||*cancel.borrow()||cancel.has_changed().is_err()||authority.read.is_revoked(){return Err(MemoryError::Denied);}
                        let allowed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN approvals a USING(operation_id) JOIN runs r USING(run_id) WHERE o.operation_id=? AND o.owner=? AND o.run_id=? AND o.environment=? AND o.policy=? AND o.state='dispatching' AND r.active=1 AND a.argument_digest=o.argument_digest AND a.environment=o.environment AND a.policy=o.policy AND a.expires_at>?)",params![op.id,c.owner,c.run_id,op.environment,c.policy,time],|r|r.get(0))?;
                        if !allowed{return Err(MemoryError::ApprovalRequired);}Ok(())
                    })
                    .map_err(error)
                    .map(|outcome| {
                        let source = if args.mutation == crate::memory::MemoryMutation::Put {
                            outcome
                                .result_version
                                .clone()
                                .map(|v| (outcome.entry_id.clone(), v))
                        } else {
                            None
                        };
                        (
                            serde_json::json!({"kind":"memory_write","data":outcome}),
                            source,
                        )
                    }),
            };
        let (value, source) = match result {
            Ok(v) => v,
            Err(e)
                if matches!(request, MemoryRequest::MemoryRead(_))
                    && !matches!(e, EngineError::Storage(_) | EngineError::Journal(_)) =>
            {
                // A local memory read has no fact-writing side effect. A live
                // scope may receive a bounded unavailable result; a withdrawn
                // scope must not publish even that response into old context.
                self.memory_grant_for(stored, op, authority.memories)?;
                check_grant(c, authority.read)?;
                if *cancel.borrow() {
                    return Err(EngineError::Forbidden);
                }
                let code = if matches!(e, EngineError::ExecutionEnvironmentUnavailable) {
                    "memory_source_unavailable"
                } else {
                    "memory_unavailable"
                };
                let value = serde_json::json!({"error":code});
                self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Durable)?;
                self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Model)?;
                let tx = rusqlite::Transaction::new_unchecked(
                    &self.connection,
                    TransactionBehavior::Immediate,
                )?;
                if tx.execute("UPDATE operations SET state='failed',error_code=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![code,encode(&value)?,op.id])?!=1{return Err(EngineError::RecoveryRequired);}
                tx.execute(
                    "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.failed')",
                    params![c.run_id, op.id],
                )?;
                self.memory_grant_for(stored, op, authority.memories)?;
                check_grant(c, authority.read)?;
                if *cancel.borrow() {
                    return Err(EngineError::Forbidden);
                }
                tx.commit()?;
                let updated = self.operation_record(c, &op.id)?;
                return self.apply_tool_context(stored, &updated);
            }
            Err(e) => {
                self.stop_agent(&c.run_id, "recovery_required", "memory_result_unconfirmed")?;
                return Err(e);
            }
        };
        self.memory_grant_for(stored, op, authority.memories)?;
        check_grant(c, authority.read)?;
        self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Durable)?;
        self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Model)?;
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some((id, version)) = source {
            tx.execute(
                "INSERT INTO runtime_memory_sources VALUES(?,?,?,?)",
                params![op.id, request.scope(), id, version],
            )?;
            self.read_memory_for_agent(
                grant,
                &id,
                Some(&version),
                0,
                4096,
                matches!(request, MemoryRequest::MemoryWrite(_)),
            )
            .map_err(error)?
            .view(self)
            .map_err(error)?;
        }
        if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![digest("memory-tool-result.v1",&value)?,encode(&value)?,encode(&value)?,op.id])?!=1{return Err(EngineError::RecoveryRequired);}
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.succeeded')",
            params![c.run_id, op.id],
        )?;
        self.memory_grant_for(stored, op, authority.memories)?;
        provenance_proof.check_after(self, changed)?;
        if *cancel.borrow() {
            return Err(EngineError::Forbidden);
        }
        tx.commit()?;
        let updated = self.operation_record(c, &op.id)?;
        self.apply_tool_context(stored, &updated)
    }
    pub fn agent_memory_outcome(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        authority: AgentAuthority<'_>,
    ) -> Result<Option<MemoryDraftOutcome>, EngineError> {
        let stored = self.stored_run(owner, run)?;
        let op = self.operation_record(&stored.config, operation)?;
        let grant = self.memory_grant_for(&stored, &op, authority.memories)?;
        let id: Option<String> = self.connection.query_row(
            "SELECT draft_id FROM runtime_memory_operations WHERE operation_id=?",
            [operation],
            |r| r.get(0),
        )?;
        id.map(|id| self.memory_draft_outcome(grant, &id).map_err(error))
            .transpose()
            .map(Option::flatten)
    }
    /// The retained reference set must exactly describe the committed result.
    /// Loading only the first row would silently discard a discovery dependency.
    pub(super) fn checked_memory_source_rows(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<Vec<(String, String, i64)>, EngineError> {
        self.intent_contract(c, op)?;
        let cached = self.checked_cached_result(c, op)?;
        let rows = {
            let mut query = self.connection.prepare("SELECT scope_id,entry_id,entry_version FROM runtime_memory_sources WHERE operation_id=? ORDER BY scope_id,entry_id LIMIT 101")?;
            query
                .query_map([&op.id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        if rows.len() > 100 {
            return Err(EngineError::RecoveryRequired);
        }
        let AgentRequest::Memory(request) = &op.request else {
            return Err(EngineError::Invalid);
        };
        let expected = if op.state == "failed"
            || matches!(request,MemoryRequest::MemoryWrite(v) if v.mutation==crate::memory::MemoryMutation::Delete)
        {
            Vec::new()
        } else if let MemoryRequest::MemoryReadV2(args) = request {
            if op.state != "succeeded" {
                return Err(EngineError::RecoveryRequired);
            }
            discovery::expected_sources(args, &cached)?
        } else {
            if op.state != "succeeded" {
                return Err(EngineError::RecoveryRequired);
            }
            let data = &cached["data"];
            let entry = data["entry_id"]
                .as_str()
                .ok_or(EngineError::RecoveryRequired)?;
            let version = match request {
                MemoryRequest::MemoryRead(args) => {
                    if args.entry_id != entry {
                        return Err(EngineError::RecoveryRequired);
                    }
                    data["version"].as_str()
                }
                MemoryRequest::MemoryWrite(_) => data["result_version"].as_str(),
                MemoryRequest::MemoryReadV2(_) => return Err(EngineError::Invalid),
            }
            .ok_or(EngineError::RecoveryRequired)?
            .parse::<i64>()
            .map_err(|_| EngineError::RecoveryRequired)?;
            if data["scope_id"].as_str() != Some(request.scope()) || version <= 0 {
                return Err(EngineError::RecoveryRequired);
            }
            vec![(request.scope().to_owned(), entry.to_owned(), version)]
        };
        if rows != expected {
            return Err(EngineError::RecoveryRequired);
        }
        Ok(rows)
    }
    pub(super) fn memory_history_version(
        &self,
        c: &RunConfig,
        scope: &str,
        entry: &str,
        observed: i64,
    ) -> Result<i64, EngineError> {
        let current:Option<i64>=self.connection.query_row("SELECT version FROM memory_entries WHERE scope_id=? AND entry_id=? AND state='active'",params![scope,entry],|r|r.get(0)).optional()?;
        let current = current.ok_or(EngineError::RecoveryRequired)?;
        if current == observed {
            return Ok(current);
        }
        if current < observed || current - observed > 100 {
            return Err(EngineError::RecoveryRequired);
        }
        let rows = {
            let mut q=self.connection.prepare("SELECT d.result_version,m.operation_id,d.draft_id,m.approval_digest FROM memory_drafts d JOIN runtime_memory_operations m USING(draft_id) JOIN operations o USING(operation_id) WHERE d.scope_id=? AND d.entry_id=? AND d.result_version>? AND d.result_version<=? AND d.state='approved' AND o.owner=? AND o.run_id=? AND o.state IN('succeeded','dispatching','unknown') ORDER BY d.result_version")?;
            q.query_map(
                params![scope, entry, observed, current, c.owner, c.run_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        if rows.len() as i64 != current - observed {
            return Err(EngineError::RecoveryRequired);
        }
        for (offset, (version, id, draft, approved)) in rows.into_iter().enumerate() {
            if version != observed + offset as i64 + 1 {
                return Err(EngineError::RecoveryRequired);
            }
            let op = self.operation_record(c, &id)?;
            self.intent_contract(c, &op)?;
            let AgentRequest::Memory(MemoryRequest::MemoryWrite(args)) = &op.request else {
                return Err(EngineError::RecoveryRequired);
            };
            if args.scope_id != scope
                || args.entry_id.as_deref() != Some(entry)
                || args.expected_version.as_deref() != Some((version - 1).to_string().as_str())
                || args.mutation != crate::memory::MemoryMutation::Put
                || !args.payload.as_ref().is_some_and(|v| v.enabled)
            {
                return Err(EngineError::RecoveryRequired);
            }
            let result = self
                .owned_memory_draft_metadata(&c.owner, scope, &draft)
                .map_err(error)?
                .ok_or(EngineError::RecoveryRequired)?;
            if result.state != "approved"
                || result.result_version.as_deref() != Some(version.to_string().as_str())
                || approved.as_deref() != Some(&result.digest)
            {
                return Err(EngineError::RecoveryRequired);
            }
            self.memory_provenance(c, &op)?;
        }
        Ok(current)
    }
    pub(super) fn revalidate_memory_sources(
        &self,
        c: &RunConfig,
        grants: &[MemoryGrant],
    ) -> Result<(), EngineError> {
        let stored = self.stored_run(&c.owner, &c.run_id)?;
        self.check_memory_bindings(&stored, grants)?;
        let ids = {
            let mut query = self.connection.prepare("SELECT operation_id FROM operations WHERE run_id=? AND state IN('succeeded','failed') AND json_extract(request_json,'$.tool') IN('memory_read','memory_read_v2','memory_write')")?;
            query
                .query_map([&c.run_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let unexpected:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_memory_sources s JOIN operations o USING(operation_id) WHERE o.run_id=? AND o.state NOT IN('succeeded','failed'))",[&c.run_id],|r|r.get(0))?;
        if unexpected {
            return Err(EngineError::RecoveryRequired);
        }
        for id in ids {
            let op = self.operation_record(c, &id)?;
            let rows = self.checked_memory_source_rows(c, &op)?;
            let cached = self.checked_cached_result(c, &op)?;
            for (scope, entry, version) in rows {
                let grant = grants
                    .iter()
                    .find(|v| v.scope_id() == scope)
                    .ok_or(EngineError::Forbidden)?;
                match &op.request {
                    AgentRequest::Memory(MemoryRequest::MemoryRead(args)) => {
                        let current = self.memory_history_version(c, &scope, &entry, version)?;
                        if current != version {
                            self.read_memory_for_agent(
                                grant,
                                &entry,
                                Some(&current.to_string()),
                                0,
                                4096,
                                false,
                            )
                            .map_err(error)?
                            .view(self)
                            .map_err(error)?;
                            continue;
                        }
                        let receipt = self
                            .read_memory_for_agent(
                                grant,
                                &entry,
                                Some(&version.to_string()),
                                args.start_byte.unwrap_or(0),
                                args.max_bytes.unwrap_or(4096),
                                false,
                            )
                            .map_err(error)?;
                        if serde_json::json!({"kind":"memory_read","data":receipt.view(self).map_err(error)?})
                            != cached
                        {
                            return Err(EngineError::RecoveryRequired);
                        }
                    }
                    AgentRequest::Memory(MemoryRequest::MemoryReadV2(args)) => {
                        let current = self.memory_history_version(c, &scope, &entry, version)?;
                        let selection = args.entry();
                        let receipt = self
                            .read_memory_for_agent(
                                grant,
                                &entry,
                                Some(&current.to_string()),
                                selection.as_ref().and_then(|v| v.start_byte).unwrap_or(0),
                                selection.as_ref().and_then(|v| v.max_bytes).unwrap_or(4096),
                                false,
                            )
                            .map_err(error)?;
                        let value = receipt.view(self).map_err(error)?;
                        if current == version {
                            discovery::check_current_projection(args, &cached, value)?;
                        }
                    }
                    AgentRequest::Memory(MemoryRequest::MemoryWrite(_)) => {
                        let current = self.memory_history_version(c, &scope, &entry, version)?;
                        self.read_memory_for_agent(
                            grant,
                            &entry,
                            Some(&current.to_string()),
                            0,
                            4096,
                            false,
                        )
                        .map_err(error)?
                        .view(self)
                        .map_err(error)?;
                        let outcome: MemoryDraftOutcome =
                            serde_json::from_value(cached["data"].clone())
                                .map_err(|_| EngineError::RecoveryRequired)?;
                        if outcome.entry_id != entry
                            || outcome.result_version.as_deref()
                                != Some(version.to_string().as_str())
                            || self
                                .memory_draft_outcome(grant, &outcome.draft_id)
                                .map_err(error)?
                                .as_ref()
                                != Some(&outcome)
                        {
                            return Err(EngineError::RecoveryRequired);
                        }
                    }
                    _ => return Err(EngineError::RecoveryRequired),
                }
            }
        }
        Ok(())
    }
}

/// An explicit recovery decision, never a model command. Stopping only needs
/// authenticated ownership; continuation requires the original current scopes.
pub enum MemoryRecovery<'a> {
    ConfirmAppliedAndStop,
    ConfirmAppliedAndContinue(AgentAuthority<'a>),
    ConfirmNotAppliedAndStop,
    ConfirmNotAppliedAndContinue(AgentAuthority<'a>),
}
impl Journal {
    pub fn agent_memory_metadata(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
    ) -> Result<Option<MemoryDraftOutcome>, EngineError> {
        let stored = self.stored_run(owner, run)?;
        let op = self.operation_record(&stored.config, operation)?;
        self.intent_contract(&stored.config, &op)?;
        let AgentRequest::Memory(MemoryRequest::MemoryWrite(args)) = &op.request else {
            return Err(EngineError::Forbidden);
        };
        let id: Option<String> = self.connection.query_row(
            "SELECT draft_id FROM runtime_memory_operations WHERE operation_id=? AND scope_id=?",
            params![op.id, args.scope_id],
            |r| r.get(0),
        )?;
        id.map(|id| {
            self.owned_memory_draft_metadata(owner, &args.scope_id, &id)
                .map_err(error)
        })
        .transpose()
        .map(Option::flatten)
    }
    pub async fn reconcile_agent_memory(
        &mut self,
        owner: &str,
        run: &str,
        operation: &str,
        expected: &str,
        decision: MemoryRecovery<'_>,
    ) -> Result<RunView, EngineError> {
        let stored = self.stored_run(owner, run)?;
        let c = &stored.config;
        let op = self.operation_record(c, operation)?;
        if !matches!(op.state.as_str(), "unknown" | "dispatching")
            || !matches!(
                stored.phase.as_str(),
                "recovery_required" | "tools_pending" | "cancelled" | "limited"
            )
        {
            return Err(EngineError::RecoveryRequired);
        }
        let (applied, authority) = match decision {
            MemoryRecovery::ConfirmAppliedAndStop => (true, None),
            MemoryRecovery::ConfirmAppliedAndContinue(v) => (true, Some(v)),
            MemoryRecovery::ConfirmNotAppliedAndStop => (false, None),
            MemoryRecovery::ConfirmNotAppliedAndContinue(v) => (false, Some(v)),
        };
        let outcome = self
            .agent_memory_metadata(owner, run, operation)?
            .ok_or(EngineError::RecoveryRequired)?;
        let (binding, entry): (Option<String>, String) = self.connection.query_row(
            "SELECT approval_digest,entry_id FROM runtime_memory_operations WHERE operation_id=?",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if outcome.digest != expected
            || binding.as_deref() != Some(expected)
            || outcome.entry_id != entry
            || if applied {
                outcome.state != "approved"
            } else {
                !matches!(outcome.state.as_str(), "pending" | "invalidated")
            }
        {
            return Err(EngineError::RecoveryRequired);
        }
        if let Some(authority) = authority {
            if !matches!(stored.phase.as_str(), "recovery_required" | "tools_pending") {
                return Err(EngineError::RecoveryRequired);
            }
            check_grant(c, authority.read)?;
            self.check_memory_bindings(&stored, authority.memories)?;
            if applied
                && let AgentRequest::Memory(MemoryRequest::MemoryWrite(args)) = &op.request
                && args.mutation == crate::memory::MemoryMutation::Put
            {
                let grant = self.memory_grant_for(&stored, &op, authority.memories)?;
                self.read_memory_for_agent(
                    grant,
                    &entry,
                    outcome.result_version.as_deref(),
                    0,
                    4096,
                    false,
                )
                .map_err(error)?
                .view(self)
                .map_err(error)?;
            }
            let _leases = self.revalidate_sources_authority(c, authority).await?;
        }
        let value = if applied {
            serde_json::json!({"kind":"memory_write","data":outcome})
        } else {
            serde_json::json!({"error":"memory_not_applied"})
        };
        self.validate_tool_output(c, &op, &value, crate::catalog::OutputChannel::Durable)?;
        self.validate_tool_output(c, &op, &value, crate::catalog::OutputChannel::Model)?;
        {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.connection,
                TransactionBehavior::Immediate,
            )?;
            if self.agent_memory_metadata(owner, run, operation)?.as_ref() != Some(&outcome) {
                return Err(EngineError::RecoveryRequired);
            }
            if applied {
                if let AgentRequest::Memory(MemoryRequest::MemoryWrite(args)) = &op.request
                    && args.mutation == crate::memory::MemoryMutation::Put
                {
                    tx.execute(
                        "INSERT OR IGNORE INTO runtime_memory_sources VALUES(?,?,?,?)",
                        params![
                            op.id,
                            outcome.scope_id,
                            entry,
                            outcome
                                .result_version
                                .as_ref()
                                .ok_or(EngineError::RecoveryRequired)?
                        ],
                    )?;
                }
                if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=?,error_code=NULL WHERE operation_id=? AND state IN('unknown','dispatching')",params![digest("memory-tool-result.v1",&value)?,encode(&value)?,encode(&value)?,op.id])?!=1{return Err(EngineError::RecoveryRequired);}
            } else {
                tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE draft_id=? AND state='pending'",[&outcome.draft_id])?;
                if tx.execute("UPDATE operations SET state='failed',result_digest=NULL,result_json=NULL,model_result=?,error_code='memory_not_applied' WHERE operation_id=? AND state IN('unknown','dispatching')",params![encode(&value)?,op.id])?!=1{return Err(EngineError::RecoveryRequired);}
            }
            tx.execute(
                "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,?)",
                params![
                    run,
                    operation,
                    if applied {
                        "memory.effect_confirmed"
                    } else {
                        "memory.no_commit_confirmed"
                    }
                ],
            )?;
            if let Some(auth) = authority {
                self.check_memory_bindings(&stored, auth.memories)?;
                check_grant(c, auth.read)?;
            }
            tx.commit()?;
        }
        if let Some(auth) = authority {
            let updated = self.operation_record(c, operation)?;
            self.apply_tool_context(&stored, &updated)?;
            if now()? >= c.deadline_unix {
                self.stop_agent(run, "limited", "deadline_after_memory_reconciliation")?;
            }
            self.agent_view_authorized_scopes(owner, run, auth).await
        } else {
            self.cancel_agent_run(owner, run)
        }
    }
}
