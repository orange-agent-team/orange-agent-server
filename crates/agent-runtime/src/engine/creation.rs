use super::*;
use crate::workspace::creation::{CreateFileArgs, CreateFileError, CreatedFile};
use sha2::Digest;
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct CreationBinding {
    pub consent_id: String,
    pub version: String,
    pub environment: String,
}
impl CreationBinding {
    pub fn from_grant(grant: &CreateFileGrant) -> Self {
        Self {
            consent_id: grant.consent_id().into(),
            version: grant.consent_version(),
            environment: grant.environment().into(),
        }
    }
}
pub(super) struct Authority<'a> {
    pub read: &'a WorkspaceGrant,
    pub create: Option<&'a CreateFileGrant>,
}
pub struct CreationApproval<'a> {
    pub operation_id: &'a str,
    pub argument_digest: &'a str,
    pub expires_at: i64,
}
pub(super) fn error(error: CreateFileError) -> EngineError {
    match error {
        CreateFileError::Journal(JournalError::ApprovalRequired) => EngineError::ApprovalRequired,
        CreateFileError::Journal(e) => EngineError::Journal(e),
        CreateFileError::Workspace(e) => EngineError::Workspace(e),
        CreateFileError::Unknown => EngineError::RecoveryRequired,
        CreateFileError::AlreadyExists => EngineError::Invalid,
        CreateFileError::Unsupported => EngineError::Forbidden,
    }
}
pub(super) fn check_pair(
    config: &RunConfig,
    read: &WorkspaceGrant,
    create: &CreateFileGrant,
) -> Result<(), EngineError> {
    if create.owner() != config.owner
        || create.policy() != config.policy
        || create.resource_identity() != read.resource_identity()
    {
        return Err(EngineError::Forbidden);
    }
    Ok(())
}
impl Journal {
    pub(super) fn creation_binding(
        &self,
        run: &str,
    ) -> Result<Option<CreationBinding>, EngineError> {
        Ok(self.connection.query_row("SELECT consent_id,consent_version,environment FROM runtime_creation_capabilities WHERE run_id=?",[run],|r|Ok(CreationBinding{consent_id:r.get(0)?,version:r.get::<_,i64>(1)?.to_string(),environment:r.get(2)?})).optional()?)
    }
    pub(super) fn check_creation_binding(
        &self,
        stored: &StoredRun,
        read: &WorkspaceGrant,
        create: Option<&CreateFileGrant>,
    ) -> Result<(), EngineError> {
        match (&stored.creation, create) {
            (None, None) => Ok(()),
            (Some(binding), Some(create)) if *binding == CreationBinding::from_grant(create) => {
                check_pair(&stored.config, read, create)?;
                self.check_creation_permission(create).map_err(error)
            }
            _ => Err(EngineError::Forbidden),
        }
    }
    /// Explicit capability negotiation; neither an old Run nor a readonly
    /// workspace silently gains write tools. Per-operation approval is separate.
    pub fn start_agent_creating(
        &mut self,
        config: &RunConfig,
        messages: Vec<Message>,
        read: &WorkspaceGrant,
        create: &CreateFileGrant,
        user_questions: bool,
    ) -> Result<RunView, EngineError> {
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
        return Err(EngineError::Forbidden);
        self.validate_creation_consent(create).map_err(error)?;
        self.start_agent_mode(config, messages, read, user_questions, Some(create))
    }
    pub async fn advance_agent_creating<G: ModelGateway>(
        &mut self,
        owner: &str,
        run: &str,
        read: &WorkspaceGrant,
        create: &CreateFileGrant,
        gateway: &G,
        cancel: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        self.advance_agent_mode(
            owner,
            run,
            Authority {
                read,
                create: Some(create),
            },
            gateway,
            cancel,
        )
        .await
    }
    pub async fn approve_agent_creation(
        &self,
        owner: &str,
        run: &str,
        approval: CreationApproval<'_>,
        read: &WorkspaceGrant,
        create: &CreateFileGrant,
    ) -> Result<(), EngineError> {
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, read)?;
        self.check_creation_binding(&stored, read, Some(create))?;
        if stored.phase != "tools_pending"
            || approval.expires_at <= now()?
            || approval.expires_at > stored.config.deadline_unix
        {
            return Err(EngineError::ApprovalRequired);
        }
        let op = self
            .pending_operations(run)?
            .into_iter()
            .find(|o| o.id == approval.operation_id && o.state == "intended")
            .ok_or(EngineError::ApprovalRequired)?;
        if !matches!(op.request, WorkspaceRequest::WriteFile(_))
            || op.digest != approval.argument_digest
        {
            return Err(EngineError::ApprovalRequired);
        }
        let _leases = self.revalidate_sources(&stored.config, read).await?;
        check_grant(&stored.config, read)?;
        self.approve_file_creation(
            create,
            &op.operation(&stored.config),
            approval.argument_digest,
            owner,
            approval.expires_at,
        )
        .map_err(error)
    }
    pub(super) async fn advance_creation_tool(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
        args: CreateFileArgs,
        authority: Authority<'_>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        let create = authority.create.ok_or(EngineError::Forbidden)?;
        let c = &stored.config;
        let _sources = tokio::select! {
            _=cancel.changed()=>{create.request_revoke();self.cancel_agent(&c.owner,&c.run_id,authority.read)?;return Ok(());}
            result=self.revalidate_sources(c,authority.read)=>result?,
        };
        self.check_creation_binding(stored, authority.read, Some(create))?;
        let operation = op.operation(c);
        let result = tokio::select! {
            _=cancel.changed()=>{create.request_revoke();self.cancel_agent(&c.owner,&c.run_id,authority.read)?;return Ok(());}
            result=self.create_file_for_agent(create,&operation,args,authority.read)=>result,
        };
        if *cancel.borrow() || authority.read.is_revoked() {
            create.request_revoke();
            self.cancel_agent(&c.owner, &c.run_id, authority.read)?;
            return Ok(());
        }
        match result {
            Ok(_) => {}
            Err(CreateFileError::Journal(JournalError::ApprovalRequired)) => {
                return Err(EngineError::ApprovalRequired);
            }
            Err(CreateFileError::Unknown) => {
                self.stop_agent(&c.run_id, "recovery_required", "creation_unknown")?;
                return Ok(());
            }
            Err(e) => {
                if self.state(&c.owner, &op.id)?.as_deref() != Some("failed") {
                    return Err(error(e));
                }
            }
        }
        let updated = self
            .pending_operations(&c.run_id)?
            .into_iter()
            .find(|o| o.id == op.id)
            .ok_or(EngineError::Invalid)?;
        self.apply_tool_context(stored, &updated)
    }
    pub(super) async fn creation_source_lease(
        &self,
        c: &RunConfig,
        read: &WorkspaceGrant,
        args: &CreateFileArgs,
        encoded: &str,
        expected: &str,
    ) -> Result<workspace::ToolReceipt, EngineError> {
        let receipt: CreatedFile =
            serde_json::from_str(encoded).map_err(|_| EngineError::Invalid)?;
        if digest("created-file.v1", &receipt)? != expected
            || receipt.path != args.path
            || receipt.sha256 != hex::encode(sha2::Sha256::digest(args.content.as_bytes()))
            || receipt.bytes != args.content.len().to_string()
        {
            return Err(EngineError::Invalid);
        }
        read.execute(
            &c.owner,
            &c.environment,
            &c.policy,
            WorkspaceRequest::ReadFile(orange_protocol::tools::ReadFileArgs {
                path: receipt.path,
                expected_sha256: Some(receipt.sha256),
                start_byte: None,
                max_bytes: None,
                start_line: None,
                end_line: None,
            }),
        )
        .await
        .map_err(Into::into)
    }
}
