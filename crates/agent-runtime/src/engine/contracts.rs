use super::*;
use crate::catalog::{CatalogDocument, CatalogError, OutputChannel, ToolCatalog, ToolPin};

fn incompatible(_: CatalogError) -> EngineError {
    EngineError::RecoveryRequired
}
pub(super) fn current(stored: &StoredRun) -> Result<ToolCatalog, EngineError> {
    if stored.memory_read_version == 2 {
        ToolCatalog::builtin_memory_discovery_scoped(
            stored.user_questions,
            stored.creation.is_some(),
            stored.editing.is_some(),
            &stored.memories,
        )
        .map_err(incompatible)
    } else if !stored.memories.is_empty() {
        ToolCatalog::builtin_memory_scoped(
            stored.user_questions,
            stored.creation.is_some(),
            stored.editing.is_some(),
            &stored.memories,
        )
        .map_err(incompatible)
    } else if stored.editing.is_some() {
        ToolCatalog::builtin_editing(stored.user_questions, stored.creation.is_some())
            .map_err(incompatible)
    } else {
        ToolCatalog::builtin(stored.user_questions, stored.creation.is_some()).map_err(incompatible)
    }
}
pub(super) fn capture(
    tx: &rusqlite::Transaction<'_>,
    invocation: &str,
    request: &GenerateRequest,
    catalog: &ToolCatalog,
) -> Result<(), EngineError> {
    if encode(&catalog.definitions().map_err(incompatible)?)? != encode(&request.tools)? {
        return Err(EngineError::Invalid);
    }
    let document = catalog.document();
    tx.execute("INSERT INTO runtime_tool_catalogs(invocation,revision,document_digest,document,origin) VALUES(?,?,?,?,'dispatch')",params![invocation,document.revision,document.document_digest,encode(&document)?])?;
    Ok(())
}
impl Journal {
    /// Reconstruct only explicitly marked legacy builtin-v1 requests. The
    /// original model request and its checksum must still match; no new tools,
    /// new input schema or changed approval may be inferred on upgrade.
    pub(super) fn invocation_catalog(
        &self,
        c: &RunConfig,
        invocation: &str,
    ) -> Result<ToolCatalog, EngineError> {
        let row:Option<(String,String)>=self.connection.query_row("SELECT i.request,i.request_digest FROM runtime_invocations i JOIN runtime_runs rr USING(run_id) JOIN runs r USING(run_id) WHERE i.invocation=? AND i.run_id=? AND r.owner=?",params![invocation,c.run_id,c.owner],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (encoded, expected) = row.ok_or(EngineError::RecoveryRequired)?;
        if encoded.len() > 256 * 1024 {
            return Err(EngineError::RecoveryRequired);
        }
        let request: GenerateRequest =
            serde_json::from_str(&encoded).map_err(|_| EngineError::RecoveryRequired)?;
        if request.invocation_id != invocation
            || request.model_version != c.model_version
            || digest("model-request-v1", &request)? != expected
            || request.validate().is_err()
        {
            return Err(EngineError::RecoveryRequired);
        }
        let saved:Option<(String,String,String)>=self.connection.query_row("SELECT revision,document_digest,document FROM runtime_tool_catalogs WHERE invocation=?",[invocation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let catalog = if let Some((revision, document_digest, raw)) = saved {
            if raw.len() > 2 * 1024 * 1024 {
                return Err(EngineError::RecoveryRequired);
            }
            let document: CatalogDocument =
                serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
            if document.revision != revision || document.document_digest != document_digest {
                return Err(EngineError::RecoveryRequired);
            }
            ToolCatalog::restore(document).map_err(incompatible)?
        } else {
            let legacy: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_catalog_legacy WHERE invocation=?)",
                [invocation],
                |r| r.get(0),
            )?;
            if !legacy {
                return Err(EngineError::RecoveryRequired);
            }
            let stored = self.stored_run(&c.owner, &c.run_id)?;
            let expected = current(&stored)?;
            if expected.document().tools.len() != request.tools.len() {
                return Err(EngineError::RecoveryRequired);
            }
            let mut specs = Vec::new();
            for definition in &request.tools {
                let mut spec = expected
                    .describe(&definition.name)
                    .ok_or(EngineError::RecoveryRequired)?
                    .clone();
                if spec.tool_version != format!("{}.v1", definition.name)
                    || spec.input_schema != definition.parameters
                {
                    return Err(EngineError::RecoveryRequired);
                }
                spec.description = definition.description.clone();
                specs.push(spec);
            }
            let catalog = ToolCatalog::new(specs).map_err(incompatible)?;
            let document = catalog.document();
            self.connection.execute("INSERT INTO runtime_tool_catalogs(invocation,revision,document_digest,document,origin) VALUES(?,?,?,?,'legacy_builtin_v1')",params![invocation,document.revision,document.document_digest,encode(&document)?])?;
            catalog
        };
        if encode(&catalog.definitions().map_err(incompatible)?)? != encode(&request.tools)? {
            return Err(EngineError::RecoveryRequired);
        }
        Ok(catalog)
    }
    pub(super) fn checked_tool_pin(
        &self,
        c: &RunConfig,
        invocation: &str,
        name: &str,
    ) -> Result<(ToolCatalog, ToolPin), EngineError> {
        let snapshot = self.invocation_catalog(c, invocation)?;
        let pin = snapshot.pin(name).map_err(incompatible)?;
        let live = current(&self.stored_run(&c.owner, &c.run_id)?)?;
        if live.pin(name).map_err(incompatible)? != pin {
            return Err(EngineError::RecoveryRequired);
        }
        Ok((snapshot, pin))
    }
    pub(super) fn intent_contract(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<(ToolCatalog, ToolPin), EngineError> {
        let (catalog, pin) = self.checked_tool_pin(c, &op.invocation, op.request.name())?;
        if pin.tool_version != op.version || op.request.digest()? != op.digest {
            return Err(EngineError::RecoveryRequired);
        }
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=? AND owner=? AND run_id=? AND invocation=? AND tool_version=? AND argument_digest=? AND environment=? AND policy=? AND requires_approval=1)",params![op.id,c.owner,c.run_id,op.invocation,op.version,op.digest,op.environment,c.policy],|r|r.get(0))?;
        if !valid
            || !catalog
                .describe(&pin.name)
                .ok_or(EngineError::RecoveryRequired)?
                .needs_approval()
        {
            return Err(EngineError::RecoveryRequired);
        }
        let row:Option<(String,String)>=self.connection.query_row("SELECT p.body,p.digest FROM responses p JOIN runtime_invocations i USING(invocation) WHERE p.invocation=? AND p.run_id=? AND i.run_id=p.run_id AND i.state='committed' AND p.attempt=i.attempt AND i.attempt=1",params![op.invocation,c.run_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (raw, expected) = row.ok_or(EngineError::RecoveryRequired)?;
        let response: ResponseCandidate =
            serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
        if digest("model-response-v1", &response)? != expected
            || response.finish_reason != "tool_calls"
        {
            return Err(EngineError::RecoveryRequired);
        }
        let calls: Vec<_> = response
            .tool_calls
            .iter()
            .filter(|call| call.id == op.call_id)
            .collect();
        let call = match calls.as_slice() {
            [call] => call,
            _ => return Err(EngineError::RecoveryRequired),
        };
        if call.name != op.request.name()
            || AgentRequest::parse(&call.name, &call.arguments)?.digest()? != op.digest
        {
            return Err(EngineError::RecoveryRequired);
        }
        catalog
            .validate_input(&pin, &call.arguments)
            .map_err(incompatible)?;
        Ok((catalog, pin))
    }
    pub(super) fn record_tool_approval_pin(
        &self,
        op: &StoredOperation,
        pin: &ToolPin,
    ) -> Result<(), EngineError> {
        self.connection.execute("INSERT INTO runtime_tool_approvals(operation_id,invocation,tool_version,execution_digest) VALUES(?,?,?,?) ON CONFLICT(operation_id) DO NOTHING",params![op.id,op.invocation,pin.tool_version,pin.execution_digest])?;
        self.require_tool_approval_pin(op, pin)
    }
    pub(super) fn require_tool_approval_pin(
        &self,
        op: &StoredOperation,
        pin: &ToolPin,
    ) -> Result<(), EngineError> {
        let found:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_tool_approvals WHERE operation_id=? AND invocation=? AND tool_version=? AND execution_digest=?)",params![op.id,op.invocation,pin.tool_version,pin.execution_digest],|r|r.get(0))?;
        if found {
            return Ok(());
        }
        let any: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_tool_approvals WHERE operation_id=?)",
            [&op.id],
            |r| r.get(0),
        )?;
        let legacy:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_catalog_legacy_approvals l JOIN approvals a USING(operation_id) WHERE l.operation_id=? AND l.invocation=? AND l.tool_version=? AND l.actor=a.actor AND l.argument_digest=a.argument_digest AND l.environment=a.environment AND l.policy=a.policy AND l.expires_at=a.expires_at)",params![op.id,op.invocation,pin.tool_version],|r|r.get(0))?;
        if any || !legacy {
            return Err(EngineError::ApprovalRequired);
        }
        self.connection.execute("INSERT INTO runtime_tool_approvals(operation_id,invocation,tool_version,execution_digest) VALUES(?,?,?,?)",params![op.id,op.invocation,pin.tool_version,pin.execution_digest])?;
        Ok(())
    }
    pub(super) fn validate_tool_output(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
        value: &serde_json::Value,
        channel: OutputChannel,
    ) -> Result<(), EngineError> {
        let (catalog, pin) = self.intent_contract(c, op)?;
        catalog
            .validate_output(&pin, value, channel)
            .map_err(incompatible)
    }
}
impl Journal {
    pub(super) fn operation_record(
        &self,
        c: &RunConfig,
        id: &str,
    ) -> Result<StoredOperation, EngineError> {
        type Row = (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        );
        let row:Option<Row>=self.connection.query_row("SELECT operation_id,invocation,request_json,argument_digest,tool_version,call_id,state,model_result,environment FROM operations WHERE operation_id=? AND run_id=? AND owner=?",params![id,c.run_id,c.owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
        let (id, invocation, request, digest, version, call_id, state, model_result, environment) =
            row.ok_or(EngineError::RecoveryRequired)?;
        Ok(StoredOperation {
            id,
            invocation,
            request: serde_json::from_str(&request).map_err(|_| EngineError::RecoveryRequired)?,
            digest,
            version,
            call_id,
            state,
            model_result,
            environment,
        })
    }
    /// Also guard trusted low-level creation calls when they target a normal
    /// Agent Run. Generic Journal adapters are not silently adopted as Runs.
    pub(crate) fn check_runtime_creation_intent(
        &self,
        operation: &Operation<'_>,
    ) -> Result<bool, EngineError> {
        let runtime: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?)",
            [operation.run],
            |r| r.get(0),
        )?;
        if !runtime {
            return Ok(false);
        }
        let stored = self.stored_run(operation.owner, operation.run)?;
        let op = self.operation_record(&stored.config, operation.id)?;
        let (_, pin) = self.intent_contract(&stored.config, &op)?;
        if op.request.name() != "write_file"
            || op.invocation != operation.invocation
            || op.digest != operation.argument_digest
            || op.version != operation.tool_version
            || op.environment != operation.environment
            || stored.config.policy != operation.policy_version
            || !operation.requires_approval
        {
            return Err(EngineError::Forbidden);
        }
        self.require_tool_approval_pin(&op, &pin)?;
        Ok(true)
    }
    pub(crate) fn check_runtime_creation_output(
        &self,
        operation: &Operation<'_>,
        result: &serde_json::Value,
    ) -> Result<(), EngineError> {
        let runtime: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?)",
            [operation.run],
            |r| r.get(0),
        )?;
        if !runtime {
            return Ok(());
        }
        let stored = self.stored_run(operation.owner, operation.run)?;
        let op = self.operation_record(&stored.config, operation.id)?;
        self.validate_tool_output(&stored.config, &op, result, OutputChannel::Durable)?;
        self.validate_tool_output(&stored.config, &op, result, OutputChannel::Model)
    }
}
impl Journal {
    pub(super) fn checked_cached_result(
        &self,
        c: &RunConfig,
        op: &StoredOperation,
    ) -> Result<serde_json::Value, EngineError> {
        type Row = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let (state,raw,expected_digest,error,model):Row=self.connection.query_row("SELECT state,result_json,result_digest,error_code,model_result FROM operations WHERE operation_id=? AND run_id=? AND owner=?",params![op.id,c.run_id,c.owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        if state != op.state || model != op.model_result {
            return Err(EngineError::RecoveryRequired);
        }
        let projected = if state == "succeeded" {
            let raw = raw.ok_or(EngineError::RecoveryRequired)?;
            let expected = expected_digest.ok_or(EngineError::RecoveryRequired)?;
            if raw.len() > 2 * 1024 * 1024 {
                return Err(EngineError::RecoveryRequired);
            }
            let raw_value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
            let contract_value = if matches!(
                op.request,
                AgentRequest::Workspace(WorkspaceRequest::ApplyPatch(_))
            ) {
                serde_json::json!({"kind":"apply_patch","data":raw_value})
            } else if matches!(
                op.request,
                AgentRequest::Workspace(WorkspaceRequest::WriteFile(_))
            ) {
                serde_json::json!({"kind":"write_file","data":raw_value})
            } else {
                raw_value
            };
            self.validate_tool_output(c, op, &contract_value, OutputChannel::Durable)?;
            if matches!(
                op.request,
                AgentRequest::Workspace(WorkspaceRequest::ApplyPatch(_))
            ) {
                let result: crate::workspace::patch::PublishedPatch =
                    serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
                if digest("published-patch.v1", &result)? != expected {
                    return Err(EngineError::RecoveryRequired);
                }
                result.model_value()
            } else if matches!(
                op.request,
                AgentRequest::Workspace(WorkspaceRequest::WriteFile(_))
            ) {
                let result: crate::workspace::creation::CreatedFile =
                    serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
                if digest("created-file.v1", &result)? != expected {
                    return Err(EngineError::RecoveryRequired);
                }
                serde_json::json!({"kind":"write_file","data":result})
            } else if matches!(op.request, AgentRequest::Memory(_)) {
                let result: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
                if digest("memory-tool-result.v1", &result)? != expected {
                    return Err(EngineError::RecoveryRequired);
                }
                result
            } else {
                let result: workspace::WorkspaceResult =
                    serde_json::from_str(&raw).map_err(|_| EngineError::RecoveryRequired)?;
                if digest("tool-result-v1", &result)? != expected {
                    return Err(EngineError::RecoveryRequired);
                }
                serde_json::from_str(&workspace::model_view(&result)?)
                    .map_err(|_| EngineError::RecoveryRequired)?
            }
        } else if state == "failed" {
            let code = error.ok_or(EngineError::RecoveryRequired)?;
            if matches!(
                op.request,
                AgentRequest::Workspace(WorkspaceRequest::WriteFile(_))
            ) {
                if code != "CREATE_REJECTED" {
                    return Err(EngineError::RecoveryRequired);
                }
                serde_json::json!({"error":"create_rejected"})
            } else {
                serde_json::json!({"error":code})
            }
        } else {
            return Err(EngineError::RecoveryRequired);
        };
        if model.as_ref().is_some_and(|text| text.len() > 128 * 1024) {
            return Err(EngineError::RecoveryRequired);
        }
        let cached: serde_json::Value =
            serde_json::from_str(model.as_ref().ok_or(EngineError::RecoveryRequired)?)
                .map_err(|_| EngineError::RecoveryRequired)?;
        if projected != cached {
            return Err(EngineError::RecoveryRequired);
        }
        self.validate_tool_output(c, op, &cached, OutputChannel::Model)?;
        Ok(cached)
    }
}
