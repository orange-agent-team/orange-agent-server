use super::*;
use crate::workspace::patch::PatchError;
#[cfg(target_vendor = "apple")]
use crate::workspace::patch::PublishedPatch;
#[cfg(target_vendor = "apple")]
use crate::workspace::patch::authority::EditFileGrant;
#[cfg(target_vendor = "apple")]
mod recovery;
#[cfg(target_vendor = "apple")]
pub use recovery::{PatchRecoveryOutcome, ResolvedPatchDecision};
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EditRequirement {
    pub consent_id: String,
    pub version: String,
    pub environment: String,
}
#[derive(Clone, Copy)]
pub(super) struct EditAuthority<'a> {
    #[cfg(target_vendor = "apple")]
    grant: Option<&'a EditFileGrant>,
    lifetime: std::marker::PhantomData<&'a ()>,
}
impl<'a> EditAuthority<'a> {
    #[cfg(target_vendor = "apple")]
    pub(super) fn from_grant(grant: &'a EditFileGrant) -> Self {
        Self {
            grant: Some(grant),
            lifetime: std::marker::PhantomData,
        }
    }

    pub(super) fn none() -> Self {
        Self {
            #[cfg(target_vendor = "apple")]
            grant: None,
            lifetime: std::marker::PhantomData,
        }
    }
}
#[cfg(target_vendor = "apple")]
#[derive(Clone, Copy)]
pub struct EditingAuthority<'a> {
    pub read: &'a WorkspaceGrant,
    pub create: Option<&'a CreateFileGrant>,
    pub edit: &'a EditFileGrant,
}
#[cfg(target_vendor = "apple")]
impl<'a> EditingAuthority<'a> {
    pub(super) fn internal(self) -> creation::Authority<'a> {
        creation::Authority {
            read: self.read,
            create: self.create,
            memories: &[],
            source_workspaces: &[],
            source_memories: &[],
            edit: EditAuthority {
                grant: Some(self.edit),
                lifetime: std::marker::PhantomData,
            },
        }
    }
}
#[cfg(target_vendor = "apple")]
pub struct EditingApproval<'a> {
    pub operation_id: &'a str,
    pub preview_digest: &'a str,
    pub expires_at: i64,
}
#[cfg(target_vendor = "apple")]
pub(super) fn error(e: PatchError) -> EngineError {
    match e {
        PatchError::Journal(JournalError::ApprovalRequired) => EngineError::ApprovalRequired,
        PatchError::Journal(e) => EngineError::Journal(e),
        PatchError::Workspace(e) => EngineError::Workspace(e),
        _ => EngineError::RecoveryRequired,
    }
}
#[cfg(target_vendor = "apple")]
pub(crate) struct HistoryAccess {
    owner: String,
    run: String,
    source: String,
    environment: String,
    policy: String,
    operation_environment: String,
}
#[cfg(target_vendor = "apple")]
impl HistoryAccess {
    pub(crate) fn coordinates(&self) -> (&str, &str, &str) {
        (&self.owner, &self.run, &self.source)
    }
    pub(crate) fn valid_for(
        &self,
        owner: &str,
        run: &str,
        source: &str,
        read: &WorkspaceGrant,
    ) -> bool {
        self.owner == owner
            && self.run == run
            && self.source == source
            && self.environment == read.environment()
            && self.policy == read.policy()
            && owner == read.owner()
    }
    pub(crate) fn operation_environment(&self) -> &str {
        &self.operation_environment
    }
}
impl Journal {
    pub fn agent_edit_requirement(
        &self,
        owner: &str,
        run: &str,
    ) -> Result<Option<EditRequirement>, EngineError> {
        Ok(self.stored_run(owner, run)?.editing)
    }
    pub(super) fn editing_binding(
        &self,
        run: &str,
    ) -> Result<Option<EditRequirement>, EngineError> {
        Ok(self.connection.query_row("SELECT consent_id,consent_version,environment FROM runtime_edit_capabilities WHERE run_id=?",[run],|r|Ok(EditRequirement {consent_id:r.get(0)?,version:r.get::<_,i64>(1)?.to_string(),environment:r.get(2)?})).optional()?)
    }
    pub(super) fn edit_start_binding(
        &self,
        c: &RunConfig,
        read: &WorkspaceGrant,
        authority: EditAuthority<'_>,
    ) -> Result<Option<EditRequirement>, EngineError> {
        #[cfg(target_vendor = "apple")]
        if let Some(edit) = authority.grant {
            self.validate_edit_consent(edit).map_err(error)?;
            if edit.owner() != c.owner
                || edit.policy() != c.policy
                || edit.resource_identity() != read.resource_identity()
            {
                return Err(EngineError::Forbidden);
            }
            return Ok(Some(EditRequirement {
                consent_id: edit.consent_id().into(),
                version: edit.consent_version().ok_or(EngineError::Forbidden)?,
                environment: edit.environment().into(),
            }));
        }
        let _ = (c, read, authority);
        Ok(None)
    }
    pub(super) fn check_edit_binding(
        &self,
        stored: &StoredRun,
        read: &WorkspaceGrant,
        authority: EditAuthority<'_>,
    ) -> Result<(), EngineError> {
        #[cfg(target_vendor = "apple")]
        if let Some(edit) = authority.grant {
            let actual = EditRequirement {
                consent_id: edit.consent_id().into(),
                version: edit.consent_version().ok_or(EngineError::Forbidden)?,
                environment: edit.environment().into(),
            };
            if stored.editing.as_ref() != Some(&actual)
                || edit.owner() != stored.config.owner
                || edit.policy() != stored.config.policy
                || edit.resource_identity() != read.resource_identity()
            {
                return Err(EngineError::Forbidden);
            }
            return self.check_edit_permission(edit).map_err(error);
        }
        let _ = (read, authority);
        if stored.editing.is_some() {
            Err(EngineError::Forbidden)
        } else {
            Ok(())
        }
    }
    #[cfg(target_vendor = "apple")]
    pub(super) fn edit_history_access(
        &self,
        c: &RunConfig,
        read: &WorkspaceGrant,
        op: &StoredOperation,
    ) -> Result<HistoryAccess, EngineError> {
        check_grant(c, read)?;
        let stored = self.stored_run(&c.owner, &c.run_id)?;
        let binding = stored.editing.as_ref().ok_or(EngineError::Forbidden)?;
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM editing_consents WHERE consent_id=? AND owner=? AND active=1 AND version=? AND environment=? AND policy=? AND resource_identity=?)",params![binding.consent_id,c.owner,binding.version,binding.environment,c.policy,read.resource_identity()],|r|r.get(0))?;
        if !valid {
            return Err(EngineError::Forbidden);
        }
        let expected = match &op.request {
            AgentRequest::Workspace(WorkspaceRequest::WriteFile(_)) => {
                &stored
                    .creation
                    .as_ref()
                    .ok_or(EngineError::Forbidden)?
                    .environment
            }
            AgentRequest::Workspace(WorkspaceRequest::ApplyPatch(_)) => &binding.environment,
            _ => &c.environment,
        };
        if &op.environment != expected {
            return Err(EngineError::Forbidden);
        }
        let (_, pin) = self.intent_contract(c, op)?;
        self.require_tool_approval_pin(op, &pin)?;
        self.checked_cached_result(c, op)?;
        Ok(HistoryAccess {
            owner: c.owner.clone(),
            run: c.run_id.clone(),
            source: op.id.clone(),
            environment: c.environment.clone(),
            policy: c.policy.clone(),
            operation_environment: op.environment.clone(),
        })
    }
    #[cfg(target_vendor = "apple")]
    pub fn start_agent_editing(
        &mut self,
        c: &RunConfig,
        messages: Vec<Message>,
        authority: EditingAuthority<'_>,
        user_questions: bool,
    ) -> Result<RunView, EngineError> {
        let authority = authority.internal();
        self.start_agent_mode(c, messages, authority, user_questions)
    }
    #[cfg(target_vendor = "apple")]
    pub async fn advance_agent_editing<G: ModelGateway>(
        &mut self,
        owner: &str,
        run: &str,
        authority: EditingAuthority<'_>,
        gateway: &G,
        cancel: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        self.advance_agent_mode(owner, run, authority.internal(), gateway, cancel)
            .await
    }
    #[cfg(target_vendor = "apple")]
    pub async fn agent_patch_preview(
        &mut self,
        owner: &str,
        run: &str,
        operation: &str,
        authority: EditingAuthority<'_>,
    ) -> Result<workspace::patch::review::PatchReview, EngineError> {
        self.agent_patch_preview_authorized(owner, run, operation, authority.internal())
            .await
    }
    #[cfg(target_vendor = "apple")]
    pub async fn agent_patch_preview_authorized(
        &mut self,
        owner: &str,
        run: &str,
        operation: &str,
        authority: AgentAuthority<'_>,
    ) -> Result<workspace::patch::review::PatchReview, EngineError> {
        let edit = authority.edit.grant.ok_or(EngineError::Forbidden)?;
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, authority.read)?;
        self.check_edit_binding(&stored, authority.read, authority.edit)?;
        self.check_creation_binding(&stored, authority.read, authority.create)?;
        if stored.phase != "tools_pending" {
            return Err(EngineError::ApprovalRequired);
        }
        let op = self
            .pending_operations(run)?
            .into_iter()
            .find(|o| o.id == operation && o.state == "intended")
            .ok_or(EngineError::ApprovalRequired)?;
        self.intent_contract(&stored.config, &op)?;
        let AgentRequest::Workspace(WorkspaceRequest::ApplyPatch(args)) = &op.request else {
            return Err(EngineError::Forbidden);
        };
        let _sources = self
            .revalidate_sources_authority(&stored.config, authority)
            .await?;
        self.prepare_patch_review(
            edit,
            authority.read,
            &op.operation(&stored.config),
            args.clone(),
            &op.call_id,
        )
        .await
        .map_err(error)
    }
    #[cfg(target_vendor = "apple")]
    pub async fn approve_agent_editing(
        &mut self,
        owner: &str,
        run: &str,
        approval: EditingApproval<'_>,
        authority: EditingAuthority<'_>,
    ) -> Result<(), EngineError> {
        self.approve_agent_editing_authorized(owner, run, approval, authority.internal())
            .await
    }
    #[cfg(target_vendor = "apple")]
    pub async fn approve_agent_editing_authorized(
        &mut self,
        owner: &str,
        run: &str,
        approval: EditingApproval<'_>,
        authority: AgentAuthority<'_>,
    ) -> Result<(), EngineError> {
        let edit = authority.edit.grant.ok_or(EngineError::Forbidden)?;
        let preview = self
            .agent_patch_preview_authorized(owner, run, approval.operation_id, authority)
            .await?;
        let stored = self.stored_run(owner, run)?;
        if preview.preview_digest != approval.preview_digest
            || approval.expires_at <= now()?
            || approval.expires_at > stored.config.deadline_unix
        {
            return Err(EngineError::ApprovalRequired);
        }
        let op = self.operation_record(&stored.config, approval.operation_id)?;
        let (_, pin) = self.intent_contract(&stored.config, &op)?;
        // Patch approval has its own transaction. Both bindings are mandatory
        // at dispatch, so a failure to save the catalog pin cannot cause effects.
        self.approve_patch_review(
            edit,
            authority.read,
            &op.operation(&stored.config),
            approval.preview_digest,
            owner,
            approval.expires_at,
        )
        .await
        .map_err(error)?;
        self.record_tool_approval_pin(&op, &pin)
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn check_runtime_edit_intent(
        &self,
        operation: &Operation<'_>,
        edit: &EditFileGrant,
        approval: bool,
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
        let binding = stored.editing.as_ref().ok_or(EngineError::Forbidden)?;
        if stored.phase != "tools_pending"
            || now()? >= stored.config.deadline_unix
            || binding.consent_id != edit.consent_id()
            || Some(binding.version.clone()) != edit.consent_version()
            || binding.environment != edit.environment()
        {
            return Err(EngineError::Forbidden);
        }
        self.check_edit_permission(edit).map_err(error)?;
        let op = self.operation_record(&stored.config, operation.id)?;
        let (_, pin) = self.intent_contract(&stored.config, &op)?;
        if !matches!(
            op.request,
            AgentRequest::Workspace(WorkspaceRequest::ApplyPatch(_))
        ) || op.invocation != operation.invocation
            || op.digest != operation.argument_digest
            || op.version != operation.tool_version
            || op.environment != operation.environment
            || stored.config.policy != operation.policy_version
            || !operation.requires_approval
        {
            return Err(EngineError::Forbidden);
        }
        if approval {
            self.require_tool_approval_pin(&op, &pin)?;
        }
        Ok(true)
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn check_runtime_edit_output(
        &self,
        owner: &str,
        run: &str,
        id: &str,
        receipt: &PublishedPatch,
    ) -> Result<(), EngineError> {
        let runtime: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?)",
            [run],
            |r| r.get(0),
        )?;
        if !runtime {
            return Ok(());
        }
        let stored = self.stored_run(owner, run)?;
        let op = self.operation_record(&stored.config, id)?;
        self.validate_tool_output(
            &stored.config,
            &op,
            &serde_json::json!({"kind":"apply_patch","data":receipt}),
            crate::catalog::OutputChannel::Durable,
        )?;
        self.validate_tool_output(
            &stored.config,
            &op,
            &receipt.model_value(),
            crate::catalog::OutputChannel::Model,
        )
    }
    pub(super) async fn advance_edit_tool(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
        authority: creation::Authority<'_>,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        self.advance_edit_tool_with_hook(stored, op, authority, cancel, |_| Ok(()))
            .await
    }
    pub(super) async fn advance_edit_tool_with_hook(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
        authority: creation::Authority<'_>,
        mut cancel: watch::Receiver<bool>,
        hook: impl FnMut(u8) -> Result<(), PatchError>,
    ) -> Result<(), EngineError> {
        #[cfg(target_vendor = "apple")]
        {
            // Blocking file publication must not freeze a current-thread host.
            if !matches!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) {
                return Err(EngineError::ExecutionEnvironmentUnavailable);
            }
            let edit = authority.edit.grant.ok_or(EngineError::Forbidden)?;
            let c = &stored.config;
            let _sources = tokio::select! {
                _=cancel.changed()=>{edit.request_revoke();self.cancel_agent(&c.owner,&c.run_id,authority.read)?;return Ok(());}
                result=self.revalidate_sources_authority(c,authority)=>result?,
            };
            self.check_edit_binding(stored, authority.read, authority.edit)?;
            let operation = op.operation(c);
            _sources.check(self)?;
            let source_guard = _sources.into_effect_guard();
            let source_observer = source_guard.observer();
            let dispatch = tokio::select! {
                _=cancel.changed()=>{edit.request_revoke();self.cancel_agent(&c.owner,&c.run_id,authority.read)?;return Ok(());}
                _=source_observer.revoked()=>{edit.request_revoke();self.stop_agent(&c.run_id,"recovery_required","source_revoked_during_edit")?;return Ok(());}
                result=self.begin_patch_publication_with_sources(edit,authority.read,&operation,Some(source_guard))=>result.map_err(error)?,
            };
            struct Watcher(tokio::task::JoinHandle<()>);
            impl Drop for Watcher {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let read_revoke = authority.read.clone();
            let edit_revoke = edit.clone();
            let mut signal = cancel.clone();
            let (catalog, pin) = self.intent_contract(c, op)?;
            let milliseconds = catalog
                .describe(&pin.name)
                .and_then(|s| s.limits.execution_timeout_ms)
                .ok_or(EngineError::Invalid)?;
            let timeout = Duration::from_millis(u64::from(milliseconds)).min(Duration::from_secs(
                c.deadline_unix.saturating_sub(now()?).max(0) as u64,
            ));
            let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let timeout_signal = timed_out.clone();
            let mut watcher = Watcher(tokio::spawn(async move {
                if !*signal.borrow() {
                    tokio::select! {
                        _=signal.changed()=>{},
                        _=source_observer.revoked()=>{},
                        _=tokio::time::sleep(timeout)=>{timeout_signal.store(true,std::sync::atomic::Ordering::Release);},
                    }
                }
                edit_revoke.request_revoke();
                read_revoke.request_revoke();
            }));
            if *cancel.borrow() || cancel.has_changed().is_err() {
                edit.request_revoke();
                authority.read.request_revoke();
            }
            let result = tokio::task::block_in_place(|| {
                self.publish_patch_with_hook(edit, authority.read, dispatch, hook)
            });
            watcher.0.abort();
            let _ = (&mut watcher.0).await;
            if timed_out.load(std::sync::atomic::Ordering::Acquire) {
                self.stop_agent(&c.run_id, "recovery_required", "edit_timeout")?;
                return Ok(());
            }
            if *cancel.borrow() || authority.read.is_revoked() {
                self.cancel_agent(&c.owner, &c.run_id, authority.read)?;
                return Ok(());
            }
            if result.is_err() {
                self.stop_agent(&c.run_id, "recovery_required", "edit_unknown")?;
                return Ok(());
            }
            let updated = self.operation_record(c, &op.id)?;
            self.apply_tool_context(stored, &updated)
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = (stored, op, authority, &mut cancel, hook);
            Err(EngineError::Forbidden)
        }
    }
}
