//! A trusted local host, not an autonomous approval authority. Each advance does
//! at most one model dispatch or one approved filesystem operation. Network model
//! implementations are injected by the application; no paid fallback is provided.
mod contracts;
mod creation;
mod editing;
pub use editing::EditRequirement;
#[cfg(target_vendor = "apple")]
pub(crate) use editing::HistoryAccess;
#[cfg(target_vendor = "apple")]
pub use editing::{EditingApproval, EditingAuthority};
mod questions;
use crate::workspace::creation::CreateFileGrant;
pub use creation::{CreationApproval, CreationRequirement};
pub use questions::PendingUserQuestion;

use crate::{
    Journal, JournalError, Operation,
    workspace::{self, WorkspaceError, WorkspaceGrant, WorkspaceRequest},
};
use orange_protocol::{
    model::{GenerateRequest, Message, ResponseCandidate, Role},
    request_digest, valid_id,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

pub type ModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResponseCandidate, GatewayFailure>> + Send + 'a>>;
/// Implemented only by trusted adapters. Implementations must never execute tools.
pub trait ModelGateway {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<'a>;
}
#[derive(Debug, thiserror::Error)]
#[error("model call did not return a validated complete response")]
pub struct GatewayFailure;
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("invalid host configuration or stored payload")]
    Invalid,
    #[error("run is unavailable to this actor")]
    Forbidden,
    #[error("operation requires explicit current user approval")]
    ApprovalRequired,
    #[error("prior execution needs reconciliation; automatic replay denied")]
    RecoveryRequired,
    #[error("host execution environment does not support this operation")]
    ExecutionEnvironmentUnavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub owner: String,
    pub run_id: String,
    pub policy: String,
    pub environment: String,
    pub model_version: String,
    pub max_steps: u32,
    pub max_invocations: u32,
    pub max_output_tokens: u32,
    pub deadline_unix: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ready,
    ModelPending,
    ToolsPending,
    AwaitingUser,
    Completed,
    Cancelled,
    Limited,
    Failed,
    RecoveryRequired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub operation_id: String,
    pub call_id: String,
    pub request: WorkspaceRequest,
    pub argument_digest: String,
    pub tool_version: String,
    pub environment: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunView {
    pub run_id: String,
    pub phase: Phase,
    pub invocations: u32,
    pub steps: u32,
    pub pending: Vec<PendingApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_question: Option<PendingUserQuestion>,
    pub final_text: Option<String>,
    pub failure: Option<String>,
}
struct StoredRun {
    config: RunConfig,
    messages: Vec<Message>,
    phase: String,
    invocations: u32,
    steps: u32,
    active: Option<String>,
    user_questions: bool,
    creation: Option<creation::CreationRequirement>,
    editing: Option<editing::EditRequirement>,
}
type StoredRunRow = (String, String, String, u32, u32, Option<String>, bool);
#[derive(Debug, Clone)]
struct StoredOperation {
    id: String,
    invocation: String,
    request: WorkspaceRequest,
    digest: String,
    version: String,
    call_id: String,
    state: String,
    model_result: Option<String>,
    environment: String,
}
impl StoredOperation {
    fn operation<'a>(&'a self, c: &'a RunConfig) -> Operation<'a> {
        Operation {
            id: &self.id,
            owner: &c.owner,
            run: &c.run_id,
            invocation: &self.invocation,
            argument_digest: &self.digest,
            tool_version: &self.version,
            environment: &self.environment,
            policy_version: &c.policy,
            requires_approval: true,
        }
    }
}
fn encode<T: Serialize>(value: &T) -> Result<String, EngineError> {
    serde_json::to_string(value).map_err(|_| EngineError::Invalid)
}
fn digest<T: Serialize>(kind: &str, value: &T) -> Result<String, EngineError> {
    request_digest(kind, value).map_err(|_| EngineError::Invalid)
}
fn now() -> Result<i64, EngineError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EngineError::Invalid)?
            .as_secs(),
    )
    .map_err(|_| EngineError::Invalid)
}

impl Journal {
    /// Registration never grants filesystem access. The independent grant must
    /// already have been created by the trusted interactive layer.
    pub fn start_agent(
        &mut self,
        config: &RunConfig,
        messages: Vec<Message>,
        grant: &WorkspaceGrant,
    ) -> Result<RunView, EngineError> {
        self.start_agent_mode(
            config,
            messages,
            grant,
            false,
            None,
            editing::EditAuthority::none(),
        )
    }
    /// Only a trusted host that can present and durably answer questions opts in.
    pub fn start_agent_interactive(
        &mut self,
        config: &RunConfig,
        messages: Vec<Message>,
        grant: &WorkspaceGrant,
    ) -> Result<RunView, EngineError> {
        self.start_agent_mode(
            config,
            messages,
            grant,
            true,
            None,
            editing::EditAuthority::none(),
        )
    }
    fn start_agent_mode(
        &mut self,
        config: &RunConfig,
        messages: Vec<Message>,
        grant: &WorkspaceGrant,
        user_questions: bool,
        creation: Option<&CreateFileGrant>,
        edit: editing::EditAuthority<'_>,
    ) -> Result<RunView, EngineError> {
        if ![
            &config.owner,
            &config.run_id,
            &config.policy,
            &config.environment,
            &config.model_version,
        ]
        .iter()
        .all(|s| valid_id(s))
            || !(1..=50).contains(&config.max_steps)
            || !(1..=100).contains(&config.max_invocations)
            || config.deadline_unix <= now()?
            || config.deadline_unix > now()?.saturating_add(86400)
            || messages.is_empty()
            || messages
                .iter()
                .any(|m| !matches!(m.role, Role::System | Role::User))
        {
            return Err(EngineError::Invalid);
        }
        check_grant(config, grant)?;
        let creation_binding = if let Some(cap) = creation {
            self.check_creation_permission(cap)
                .map_err(creation::error)?;
            creation::check_pair(config, grant, cap)?;
            Some(creation::CreationRequirement::from_grant(cap))
        } else {
            None
        };
        let edit_binding = self.edit_start_binding(config, grant, edit)?;
        let config_json = encode(config)?;
        let config_digest = if let Some(binding) = &edit_binding {
            digest(
                "run-config-edit-v1",
                &(
                    config,
                    &messages,
                    user_questions,
                    &creation_binding,
                    binding,
                ),
            )?
        } else if let Some(binding) = &creation_binding {
            digest(
                "run-config-create-v1",
                &(config, &messages, user_questions, binding),
            )?
        } else if user_questions {
            digest("run-config-interactive-v1", &(config, &messages))?
        } else {
            digest("run-config-v1", &(config, &messages))?
        };
        let request = make_request_mode(
            config,
            "validation",
            messages.clone(),
            user_questions,
            creation_binding.is_some(),
            edit_binding.is_some(),
        );
        request.validate().map_err(|_| EngineError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let registered:Option<(String,bool)>=tx.query_row("SELECT workspace_id,active FROM local_workspaces WHERE owner=? AND environment=? AND policy=?",params![config.owner,config.environment,config.policy],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if registered.as_ref().is_some_and(|(_, active)| !*active) {
            return Err(JournalError::Revoked.into());
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT config_digest FROM runtime_runs WHERE run_id=?",
                [&config.run_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            if old != config_digest {
                return Err(JournalError::Conflict.into());
            }
            tx.commit()?;
            return self.agent_view(&config.owner, &config.run_id);
        }
        let quarantine:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE environment=?1 AND state IN('unknown','dispatching') UNION SELECT 1 FROM runtime_runs WHERE environment=?1 AND phase='recovery_required' UNION SELECT 1 FROM runtime_invocations i JOIN runtime_runs r USING(run_id) WHERE r.environment=?1 AND i.state IN('unknown','dispatched'))",[&config.environment],|r|r.get(0))?;
        if quarantine {
            return Err(EngineError::RecoveryRequired);
        }
        if registered.is_some()
            && crate::workspaces::quarantined(&tx, &config.owner, grant.resource_identity())?
        {
            return Err(EngineError::RecoveryRequired);
        }
        tx.execute(
            "INSERT INTO runs(run_id,owner,policy,active) VALUES(?,?,?,1)",
            params![config.run_id, config.owner, config.policy],
        )?;
        tx.execute("INSERT INTO runtime_runs(run_id,config,config_digest,environment,messages,phase) VALUES(?,?,?,?,?,'ready')",params![config.run_id,config_json,config_digest,config.environment,encode(&messages)?])?;
        if user_questions {
            tx.execute(
                "INSERT INTO runtime_capabilities(run_id,user_questions) VALUES(?,1)",
                [&config.run_id],
            )?;
        }
        if let Some(binding) = &creation_binding {
            tx.execute("INSERT INTO runtime_creation_capabilities(run_id,consent_id,consent_version,environment) VALUES(?,?,?,?)",params![config.run_id,binding.consent_id,binding.version,binding.environment])?;
        }
        if let Some(binding) = &edit_binding {
            tx.execute("INSERT INTO runtime_edit_capabilities(run_id,consent_id,consent_version,environment) VALUES(?,?,?,?)",params![config.run_id,binding.consent_id,binding.version,binding.environment])?;
        }
        if let Some((workspace, _)) = registered {
            tx.execute(
                "INSERT INTO run_workspaces(run_id,workspace_id) VALUES(?,?)",
                params![config.run_id, workspace],
            )?;
        }
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,'run.started')",
            [&config.run_id],
        )?;
        tx.commit()?;
        self.agent_view(&config.owner, &config.run_id)
    }
    fn stored_run(&self, owner: &str, run: &str) -> Result<StoredRun, EngineError> {
        let row:Option<StoredRunRow>=self.connection.query_row("SELECT config,messages,phase,invocations,steps,active_invocation,COALESCE((SELECT user_questions FROM runtime_capabilities c WHERE c.run_id=runtime_runs.run_id),0) FROM runtime_runs JOIN runs USING(run_id) WHERE run_id=? AND owner=?",params![run,owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let (config, messages, phase, invocations, steps, active, user_questions) =
            row.ok_or(EngineError::Forbidden)?;
        Ok(StoredRun {
            config: serde_json::from_str(&config).map_err(|_| EngineError::Invalid)?,
            messages: serde_json::from_str(&messages).map_err(|_| EngineError::Invalid)?,
            phase,
            invocations,
            steps,
            active,
            user_questions,
            creation: self.creation_binding(run)?,
            editing: self.editing_binding(run)?,
        })
    }
    fn pending_operations(&self, run: &str) -> Result<Vec<StoredOperation>, EngineError> {
        let mut query=self.connection.prepare("SELECT operation_id,invocation,request_json,argument_digest,tool_version,call_id,state,model_result,environment FROM operations WHERE run_id=? AND context_applied=0 AND request_json IS NOT NULL ORDER BY rowid")?;
        let rows = query.query_map([run], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                invocation,
                request,
                digest,
                version,
                call_id,
                state,
                model_result,
                environment,
            ) = row?;
            Ok(StoredOperation {
                id,
                invocation,
                request: serde_json::from_str(&request).map_err(|_| EngineError::Invalid)?,
                digest,
                version,
                call_id,
                state,
                model_result,
                environment,
            })
        })
        .collect()
    }
    pub fn agent_view(&self, owner: &str, run: &str) -> Result<RunView, EngineError> {
        let stored = self.stored_run(owner, run)?;
        let pending = self
            .pending_operations(run)?
            .into_iter()
            .filter(|o| o.state == "intended")
            .map(|o| PendingApproval {
                operation_id: o.id,
                call_id: o.call_id,
                request: o.request,
                argument_digest: o.digest,
                tool_version: o.version,
                environment: o.environment,
            })
            .collect();
        let (final_text, failure, active) = self.connection.query_row(
            "SELECT final_text,failure,active FROM runtime_runs JOIN runs USING(run_id) WHERE run_id=?",
            [run],
            |r| Ok((r.get(0)?, r.get(1)?,r.get::<_,bool>(2)?)),
        )?;
        let user_question = if active && stored.phase == "awaiting_user" {
            Some(
                self.pending_user_question(&stored)?
                    .ok_or(EngineError::Invalid)?,
            )
        } else {
            None
        };
        let phase = serde_json::from_value(serde_json::Value::String(stored.phase))
            .map_err(|_| EngineError::Invalid)?;
        Ok(RunView {
            run_id: run.into(),
            phase,
            invocations: stored.invocations,
            steps: stored.steps,
            pending: if active { pending } else { vec![] },
            user_question,
            final_text: if active { final_text } else { None },
            failure,
        })
    }
    /// Content-bearing UI/recovery reads require a current workspace capability
    /// and current source versions. Metadata-only run summaries remain separate.
    pub async fn agent_view_authorized(
        &self,
        owner: &str,
        run: &str,
        grant: &WorkspaceGrant,
    ) -> Result<RunView, EngineError> {
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, grant)?;
        let view = self.agent_view(owner, run)?;
        if view.final_text.is_some() || !view.pending.is_empty() || view.user_question.is_some() {
            let _leases = self.revalidate_sources(&stored.config, grant).await?;
            for op in self
                .pending_operations(run)?
                .iter()
                .filter(|op| op.state == "intended")
            {
                self.intent_contract(&stored.config, op)?;
            }
            check_grant(&stored.config, grant)?;
            return self.agent_view(owner, run);
        }
        Ok(view)
    }
    /// Trusted hosts use this content-free configuration to select the exact
    /// model/policy on recovery. It does not grant workspace or tool access.
    pub fn agent_config(&self, owner: &str, run: &str) -> Result<RunConfig, EngineError> {
        Ok(self.stored_run(owner, run)?.config)
    }
    pub fn agent_workspace_id(
        &self,
        owner: &str,
        run: &str,
    ) -> Result<Option<String>, EngineError> {
        self.stored_run(owner, run)?;
        Ok(self
            .connection
            .query_row(
                "SELECT workspace_id FROM run_workspaces WHERE run_id=?",
                [run],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Owner is authenticated by the host; the model never receives this method.
    pub fn approve_agent_tool(
        &self,
        owner: &str,
        run: &str,
        operation_id: &str,
        expires_at: i64,
        grant: &WorkspaceGrant,
    ) -> Result<(), EngineError> {
        let stored = self.stored_run(owner, run)?;
        check_grant(&stored.config, grant)?;
        if stored.phase != "tools_pending"
            || expires_at <= now()?
            || expires_at > stored.config.deadline_unix
        {
            return Err(EngineError::ApprovalRequired);
        }
        let op = self
            .pending_operations(run)?
            .into_iter()
            .find(|o| o.id == operation_id && o.state == "intended")
            .ok_or(EngineError::ApprovalRequired)?;
        if matches!(
            op.request,
            WorkspaceRequest::WriteFile(_) | WorkspaceRequest::ApplyPatch(_)
        ) {
            return Err(EngineError::ApprovalRequired);
        }
        let (_, pin) = self.intent_contract(&stored.config, &op)?;
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        self.approve(&op.operation(&stored.config), owner, expires_at)?;
        self.record_tool_approval_pin(&op, &pin)?;
        tx.commit()?;
        Ok(())
    }
    pub fn cancel_agent(
        &mut self,
        owner: &str,
        run: &str,
        grant: &WorkspaceGrant,
    ) -> Result<RunView, EngineError> {
        let stored = self.stored_run(owner, run)?;
        if grant.owner() != owner || grant.environment() != stored.config.environment {
            return Err(EngineError::Forbidden);
        }
        grant.request_revoke();
        self.cancel_agent_run(owner, run)
    }
    /// Revoking an owned Run never requires fresh filesystem access. The host
    /// must also signal any in-flight invocation before waiting for this writer.
    pub fn cancel_agent_run(&mut self, owner: &str, run: &str) -> Result<RunView, EngineError> {
        self.stored_run(owner, run)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE runs SET active=0 WHERE run_id=?", [run])?;
        tx.execute(
            "UPDATE runtime_invocations SET state='unknown' WHERE run_id=? AND state='dispatched'",
            [run],
        )?;
        tx.execute(
            "UPDATE operations SET state='unknown' WHERE run_id=? AND state='dispatching'",
            [run],
        )?;
        tx.execute("UPDATE runtime_runs SET phase='cancelled',failure='cancelled' WHERE run_id=? AND phase!='completed'",[run])?;
        tx.execute(
            "UPDATE runtime_questions SET state='cancelled' WHERE run_id=? AND state='pending'",
            [run],
        )?;
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,'run.cancelled')",
            [run],
        )?;
        tx.commit()?;
        self.agent_view(owner, run)
    }
    fn stop_agent(&mut self, run: &str, phase: &str, reason: &str) -> Result<(), EngineError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE runtime_runs SET phase=?,failure=? WHERE run_id=?",
            params![phase, reason, run],
        )?;
        tx.execute(
            "UPDATE runtime_invocations SET state='unknown' WHERE run_id=? AND state='dispatched'",
            [run],
        )?;
        tx.execute(
            "UPDATE operations SET state='unknown' WHERE run_id=? AND state='dispatching'",
            [run],
        )?;
        tx.execute(
            "UPDATE runtime_questions SET state='cancelled' WHERE run_id=? AND state='pending'",
            [run],
        )?;
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,?)",
            params![run, format!("run.{phase}")],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// One bounded transition. Dropping this future leaves a durable dispatch
    /// marker; the next advance/reopen refuses to replay that invocation.
    pub async fn advance_agent<G: ModelGateway>(
        &mut self,
        owner: &str,
        run: &str,
        grant: &WorkspaceGrant,
        gateway: &G,
        cancellation: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        self.advance_agent_mode(
            owner,
            run,
            creation::Authority {
                read: grant,
                create: None,
                edit: editing::EditAuthority::none(),
            },
            gateway,
            cancellation,
        )
        .await
    }
    async fn advance_agent_mode<G: ModelGateway>(
        &mut self,
        owner: &str,
        run: &str,
        authority: creation::Authority<'_>,
        gateway: &G,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        let grant = authority.read;
        let stored = self.stored_run(owner, run)?;
        if !matches!(
            stored.phase.as_str(),
            "ready" | "model_pending" | "tools_pending" | "awaiting_user"
        ) {
            let view = self.agent_view(owner, run)?;
            if view.final_text.is_some() || !view.pending.is_empty() || view.user_question.is_some()
            {
                return self.agent_view_authorized(owner, run, grant).await;
            }
            return Ok(view);
        }
        self.check_creation_binding(&stored, grant, authority.create)?;
        self.check_edit_binding(&stored, grant, authority.edit)?;
        if *cancellation.borrow() || cancellation.has_changed().is_err() || grant.is_revoked() {
            return self.cancel_agent(owner, run, grant);
        }
        check_grant(&stored.config, grant)?;
        let active: bool =
            self.connection
                .query_row("SELECT active FROM runs WHERE run_id=?", [run], |r| {
                    r.get(0)
                })?;
        if !active {
            return self.cancel_agent(owner, run, grant);
        }
        if now()? >= stored.config.deadline_unix {
            self.stop_agent(run, "limited", "deadline")?;
            return self.agent_view(owner, run);
        }
        if stored.phase == "awaiting_user" {
            return self.agent_view_authorized(owner, run, grant).await;
        }
        if stored.phase == "tools_pending" {
            self.advance_tool(stored, authority, cancellation).await?;
            return self.agent_view(owner, run);
        }
        if stored.phase == "model_pending" {
            // The current host only leaves this state after a future is dropped.
            self.stop_agent(run, "recovery_required", "interrupted_model")?;
            grant.request_revoke();
            return self.agent_view(owner, run);
        }
        if stored.steps >= stored.config.max_steps
            || stored.invocations >= stored.config.max_invocations
        {
            self.stop_agent(run, "limited", "execution_budget")?;
            return self.agent_view(owner, run);
        }
        // Revalidate every source before reusing cached content. The result leases
        // remain held through model submission; changed/deleted sources fail closed.
        let lineage = tokio::select! {
            _=cancellation.changed()=>{return self.cancel_agent(owner,run,grant);}
            result=tokio::time::timeout(Duration::from_secs(stored.config.deadline_unix.saturating_sub(now()?).max(0) as u64),self.revalidate_sources(&stored.config,grant))=>result.unwrap_or(Err(EngineError::RecoveryRequired)),
        };
        let _leases = match lineage {
            Ok(value) => value,
            Err(_) => {
                self.stop_agent(run, "recovery_required", "source_changed")?;
                return self.agent_view(owner, run);
            }
        };
        if *cancellation.borrow() || cancellation.has_changed().is_err() || grant.is_revoked() {
            return self.cancel_agent(owner, run, grant);
        }
        let invocation = format!("inv-{}", uuid::Uuid::new_v4());
        if now()? >= stored.config.deadline_unix {
            self.stop_agent(run, "limited", "deadline")?;
            return self.agent_view(owner, run);
        }
        let catalog = contracts::current(&stored)?;
        let request = make_request_mode(
            &stored.config,
            &invocation,
            stored.messages.clone(),
            stored.user_questions,
            stored.creation.is_some(),
            stored.editing.is_some(),
        );
        if request.validate().is_err() {
            self.stop_agent(run, "limited", "context_budget")?;
            return self.agent_view(owner, run);
        }
        let epoch: i64 = self.connection.query_row(
            "SELECT epoch FROM runtime_host WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO runtime_invocations(invocation,run_id,ordinal,request,request_digest,state,epoch,fencing,attempt) VALUES(?,?,?,?,?,'dispatched',?,?,1)",params![invocation,run,stored.invocations+1,encode(&request)?,digest("model-request-v1",&request)?,epoch,stored.invocations+1])?;
        contracts::capture(&tx, &invocation, &request, &catalog)?;
        tx.execute("UPDATE runtime_runs SET phase='model_pending',active_invocation=?,invocations=invocations+1,steps=steps+1 WHERE run_id=? AND phase='ready'",params![invocation,run])?;
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,'model.dispatched')",
            [run],
        )?;
        tx.commit()?;
        let remaining = stored
            .config
            .deadline_unix
            .saturating_sub(now()?)
            .clamp(0, 120) as u64;
        let adapter_cancellation = cancellation.clone();
        let result = tokio::select! {
            _=cancellation.changed()=>{return self.cancel_agent(owner,run,grant);}
            result=tokio::time::timeout(Duration::from_secs(remaining),gateway.generate(&request,adapter_cancellation))=>result,
        };
        if *cancellation.borrow() || grant.is_revoked() {
            return self.cancel_agent(owner, run, grant);
        }
        if now()? >= stored.config.deadline_unix {
            self.stop_agent(run, "recovery_required", "model_deadline")?;
            return self.agent_view(owner, run);
        }
        match result {
            Ok(Ok(candidate)) => {
                if let Err(error) =
                    self.accept_candidate(&stored, &invocation, epoch, &candidate, authority)
                {
                    self.stop_agent(run, "recovery_required", "invalid_model_response")?;
                    return Err(error);
                }
            }
            _ => self.stop_agent(run, "recovery_required", "model_unknown")?,
        }
        self.agent_view(owner, run)
    }
    fn accept_candidate(
        &mut self,
        stored: &StoredRun,
        invocation: &str,
        epoch: i64,
        candidate: &ResponseCandidate,
        authority: creation::Authority<'_>,
    ) -> Result<(), EngineError> {
        let grant = authority.read;
        check_grant(&stored.config, grant)?;
        self.check_creation_binding(stored, grant, authority.create)?;
        self.check_edit_binding(stored, grant, authority.edit)?;
        if !valid_id(&candidate.response_id)
            || candidate.tool_calls.len() > 80
            || encode(candidate)?.len() > 256 * 1024
            || (candidate.tool_calls.is_empty() && candidate.finish_reason != "stop")
            || (!candidate.tool_calls.is_empty() && candidate.finish_reason != "tool_calls")
            || candidate.native_usage.as_ref().is_some_and(|usage| {
                usage.output_tokens > u64::from(stored.config.max_output_tokens)
            })
        {
            return Err(EngineError::Invalid);
        }
        let catalog = self.invocation_catalog(&stored.config, invocation)?;
        let mut ids = std::collections::BTreeSet::new();
        let mut requests = Vec::new();
        let mut question = None;
        for call in &candidate.tool_calls {
            if !valid_id(&call.id)
                || !ids.insert(&call.id)
                || stored
                    .messages
                    .iter()
                    .flat_map(|m| &m.tool_calls)
                    .any(|old| old.id == call.id)
            {
                return Err(EngineError::Invalid);
            }
            let pin = catalog
                .pin(&call.name)
                .map_err(|_| EngineError::Forbidden)?;
            catalog
                .validate_input(&pin, &call.arguments)
                .map_err(|_| EngineError::Invalid)?;
            if call.name == "ask_user" {
                if !stored.user_questions || candidate.tool_calls.len() != 1 {
                    return Err(EngineError::Invalid);
                }
                question = Some(
                    orange_protocol::questions::AskUser::parse(&call.arguments)
                        .ok_or(EngineError::Invalid)?,
                );
            } else {
                let request = WorkspaceRequest::parse(&call.name, &call.arguments)?;
                if matches!(request, WorkspaceRequest::WriteFile(_)) && stored.creation.is_none() {
                    return Err(EngineError::Forbidden);
                }
                if matches!(request, WorkspaceRequest::ApplyPatch(_)) && stored.editing.is_none() {
                    return Err(EngineError::Forbidden);
                }
                requests.push(request);
            }
        }
        let c = &stored.config;
        let mut messages = stored.messages.clone();
        messages.push(Message {
            role: Role::Assistant,
            content: candidate.text.clone(),
            tool_calls: candidate.tool_calls.clone(),
            tool_call_id: None,
        });
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_invocations i JOIN runs r USING(run_id) JOIN runtime_runs rr USING(run_id) JOIN runtime_host h ON h.singleton=1 WHERE i.invocation=? AND i.run_id=? AND i.state='dispatched' AND i.epoch=? AND h.epoch=i.epoch AND i.attempt=1 AND r.active=1 AND rr.active_invocation=i.invocation AND rr.phase='model_pending')",params![invocation,c.run_id,epoch],|r|r.get(0))?;
        if !valid {
            return Err(EngineError::RecoveryRequired);
        }
        tx.execute(
            "INSERT INTO responses(invocation,run_id,attempt,digest,body) VALUES(?,?,1,?,?)",
            params![
                invocation,
                c.run_id,
                digest("model-response-v1", candidate)?,
                encode(candidate)?
            ],
        )?;
        if let Some(args) = &question {
            questions::commit_question(&tx, c, invocation, &candidate.tool_calls[0].id, args)?;
        }
        for (ordinal, (call, request)) in candidate.tool_calls.iter().zip(requests).enumerate() {
            let ordinal = i64::try_from(ordinal).map_err(|_| EngineError::Invalid)?;
            let id = format!("op-{}", uuid::Uuid::new_v4());
            let environment = if matches!(request, WorkspaceRequest::WriteFile(_)) {
                stored
                    .creation
                    .as_ref()
                    .ok_or(EngineError::Forbidden)?
                    .environment
                    .as_str()
            } else if matches!(request, WorkspaceRequest::ApplyPatch(_)) {
                stored
                    .editing
                    .as_ref()
                    .ok_or(EngineError::Forbidden)?
                    .environment
                    .as_str()
            } else {
                c.environment.as_str()
            };
            tx.execute("INSERT INTO operations(operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state,request_json,call_id,ordinal) VALUES(?,?,?,?,?,?,?,?,1,'intended',?,?,?)",params![id,c.owner,c.run_id,invocation,request.digest()?,request.version(),environment,c.policy,encode(&request)?,call.id,ordinal])?;
            tx.execute(
                "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.intended')",
                params![c.run_id, id],
            )?;
        }
        let done = candidate.tool_calls.is_empty();
        tx.execute(
            "UPDATE runtime_invocations SET state='committed' WHERE invocation=?",
            [invocation],
        )?;
        tx.execute(
            "UPDATE runtime_runs SET messages=?,phase=?,final_text=? WHERE run_id=?",
            params![
                encode(&messages)?,
                if done {
                    "completed"
                } else if question.is_some() {
                    "awaiting_user"
                } else {
                    "tools_pending"
                },
                if done { Some(&candidate.text) } else { None },
                c.run_id
            ],
        )?;
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,'model.committed')",
            [&c.run_id],
        )?;
        tx.commit()?;
        Ok(())
    }
    async fn advance_tool(
        &mut self,
        stored: StoredRun,
        authority: creation::Authority<'_>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        let grant = authority.read;
        let c = &stored.config;
        let operations = self.pending_operations(&c.run_id)?;
        let Some(op) = operations.first() else {
            return Err(EngineError::Invalid);
        };
        if matches!(op.state.as_str(), "unknown" | "dispatching") {
            self.stop_agent(&c.run_id, "recovery_required", "interrupted_tool")?;
            grant.request_revoke();
            return Ok(());
        }
        if matches!(op.state.as_str(), "succeeded" | "failed") {
            return self.apply_tool_context(&stored, op);
        }
        if stored.steps >= c.max_steps {
            self.stop_agent(&c.run_id, "limited", "execution_budget")?;
            return Ok(());
        }
        let (_, pin) = self.intent_contract(c, op)?;
        self.require_tool_approval_pin(op, &pin)?;
        if let WorkspaceRequest::WriteFile(args) = &op.request {
            return self
                .advance_creation_tool(&stored, op, args.clone(), authority, cancellation)
                .await;
        }
        if matches!(op.request, WorkspaceRequest::ApplyPatch(_)) {
            return self
                .advance_edit_tool(&stored, op, authority, cancellation)
                .await;
        }
        // Capacity acquisition may wait, but has no filesystem effect before claim.
        let prepared = tokio::select! {
            _=cancellation.changed()=>{self.cancel_agent(&c.owner,&c.run_id,grant)?;return Ok(());}
            result=grant.prepare(&c.owner,&c.environment,&c.policy,op.request.clone())=>result?,
        };
        if *cancellation.borrow() || !prepared.is_authorized() {
            self.cancel_agent(&c.owner, &c.run_id, grant)?;
            return Ok(());
        }
        if now()? >= c.deadline_unix {
            self.stop_agent(&c.run_id, "limited", "deadline")?;
            return Ok(());
        }
        match self.claim(&op.operation(c), now()?) {
            Err(JournalError::ApprovalRequired) => return Err(EngineError::ApprovalRequired),
            other => other?,
        }
        self.connection.execute(
            "UPDATE runtime_runs SET steps=steps+1 WHERE run_id=?",
            [&c.run_id],
        )?;
        let result = tokio::select! {
            _=cancellation.changed()=>{self.cancel_agent(&c.owner,&c.run_id,grant)?;return Ok(());}
            result=prepared.execute()=>result,
        };
        if *cancellation.borrow() || grant.is_revoked() {
            self.cancel_agent(&c.owner, &c.run_id, grant)?;
            return Ok(());
        }
        match result {
            Ok(receipt) => {
                self.validate_tool_output(
                    c,
                    op,
                    &serde_json::to_value(receipt.result()).map_err(|_| EngineError::Invalid)?,
                    crate::catalog::OutputChannel::Durable,
                )?;
                self.validate_tool_output(
                    c,
                    op,
                    &serde_json::from_str(receipt.model_text())
                        .map_err(|_| EngineError::Invalid)?,
                    crate::catalog::OutputChannel::Model,
                )?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![digest("tool-result-v1",receipt.result())?,encode(receipt.result())?,receipt.model_text(),op.id])?!=1{return Err(EngineError::RecoveryRequired);}
                tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.succeeded')",params![c.run_id,op.id])?;
                tx.commit()?;
                // Receipt keeps the authorization lease until this commit is durable.
            }
            Err(WorkspaceError::Timeout | WorkspaceError::Interrupted) => {
                self.stop_agent(&c.run_id, "recovery_required", "tool_unknown")?
            }
            Err(error) => {
                let code = tool_error_code(&error);
                self.validate_tool_output(
                    c,
                    op,
                    &serde_json::json!({"error":code}),
                    crate::catalog::OutputChannel::Model,
                )?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute("UPDATE operations SET state='failed',error_code=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![code,encode(&serde_json::json!({"error":code}))?,op.id])?;
                tx.execute(
                    "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.failed')",
                    params![c.run_id, op.id],
                )?;
                tx.commit()?;
            }
        }
        // Context is a separate recoverable transaction. No repeated tool effect.
        if let Some(updated) = self
            .pending_operations(&c.run_id)?
            .first()
            .filter(|o| matches!(o.state.as_str(), "succeeded" | "failed"))
        {
            self.apply_tool_context(&stored, updated)?;
        }
        Ok(())
    }
    fn apply_tool_context(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
    ) -> Result<(), EngineError> {
        if stored.active.as_deref() != Some(&op.invocation) {
            return Err(EngineError::Invalid);
        }
        self.checked_cached_result(&stored.config, op)?;
        let mut messages = stored.messages.clone();
        messages.push(Message {
            role: Role::Tool,
            content: op.model_result.clone().ok_or(EngineError::Invalid)?,
            tool_calls: vec![],
            tool_call_id: Some(op.call_id.clone()),
        });
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE operations SET context_applied=1 WHERE operation_id=? AND context_applied=0 AND state IN('succeeded','failed')",[&op.id])?!=1{return Err(EngineError::RecoveryRequired);}
        let remaining: i64 = tx.query_row(
            "SELECT count(*) FROM operations WHERE invocation=? AND context_applied=0",
            [&op.invocation],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE runtime_runs SET messages=?,phase=? WHERE run_id=?",
            params![
                encode(&messages)?,
                if remaining == 0 {
                    "ready"
                } else {
                    "tools_pending"
                },
                stored.config.run_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    async fn revalidate_sources(
        &self,
        c: &RunConfig,
        grant: &WorkspaceGrant,
    ) -> Result<Vec<workspace::ToolReceipt>, EngineError> {
        let rows = {
            let mut statement=self.connection.prepare("SELECT request_json,result_digest,result_json,operation_id FROM operations WHERE run_id=? AND state='succeeded' AND request_json IS NOT NULL")?;
            statement
                .query_map([&c.run_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut leases = Vec::new();
        for (request, expected, encoded, id) in rows {
            let op = self.operation_record(c, &id)?;
            self.intent_contract(c, &op)?;
            self.checked_cached_result(c, &op)?;
            if self.editing_binding(&c.run_id)?.is_some() {
                #[cfg(target_vendor = "apple")]
                {
                    let access = self.edit_history_access(c, grant, &op)?;
                    leases.push(
                        self.revalidate_agent_history(access, grant)
                            .await
                            .map_err(editing::error)?,
                    );
                    continue;
                }
                #[cfg(not(target_vendor = "apple"))]
                return Err(EngineError::Forbidden);
            }
            let request: WorkspaceRequest =
                serde_json::from_str(&request).map_err(|_| EngineError::Invalid)?;
            if let WorkspaceRequest::WriteFile(args) = &request {
                leases.push(
                    self.creation_source_lease(c, grant, args, &encoded, &expected)
                        .await?,
                );
                continue;
            }
            let receipt = grant
                .execute(&c.owner, &c.environment, &c.policy, request)
                .await?;
            if digest("tool-result-v1", receipt.result())? != expected {
                return Err(WorkspaceError::VersionChanged.into());
            }
            leases.push(receipt);
        }
        Ok(leases)
    }
}
fn make_request_mode(
    c: &RunConfig,
    invocation: &str,
    messages: Vec<Message>,
    user_questions: bool,
    create_files: bool,
    edit_files: bool,
) -> GenerateRequest {
    let mut tools = workspace::definitions();
    if user_questions {
        tools.push(orange_protocol::questions::definition());
    }
    if create_files {
        tools.push(workspace::creation::definition());
    }
    if edit_files {
        tools.push(workspace::patch::definition());
    }
    GenerateRequest {
        invocation_id: invocation.into(),
        model_version: c.model_version.clone(),
        messages,
        tools,
        max_output_tokens: c.max_output_tokens,
    }
}
fn check_grant(c: &RunConfig, grant: &WorkspaceGrant) -> Result<(), EngineError> {
    if grant.is_revoked()
        || grant.owner() != c.owner
        || grant.environment() != c.environment
        || grant.policy() != c.policy
    {
        return Err(EngineError::Forbidden);
    }
    Ok(())
}
fn tool_error_code(error: &WorkspaceError) -> &'static str {
    match error {
        WorkspaceError::Invalid => "invalid_arguments",
        WorkspaceError::Forbidden => "forbidden",
        WorkspaceError::Revoked => "revoked",
        WorkspaceError::EnvironmentChanged => "environment_changed",
        WorkspaceError::PersistentIdentityUnavailable => "persistent_identity_unavailable",
        WorkspaceError::Limit | WorkspaceError::FileTooLarge => "limit",
        WorkspaceError::VersionChanged => "version_changed",
        WorkspaceError::SnapshotChanged => "snapshot_changed",
        WorkspaceError::NotText => "not_text",
        WorkspaceError::Io(_) => "filesystem_error",
        WorkspaceError::Interrupted | WorkspaceError::Timeout => "unknown",
    }
}

#[cfg(test)]
mod tests;
