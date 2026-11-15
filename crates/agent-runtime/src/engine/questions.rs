//! A user reply is context data, never an operation approval.
use super::*;
use orange_protocol::questions::{AnswerUser, AskUser, UserQuestion, valid_answers};
use orange_protocol::valid_digest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUserQuestion {
    pub question_id: String,
    pub argument_digest: String,
    pub questions: Vec<UserQuestion>,
}
fn question_digest(config: &RunConfig, body: &AskUser) -> Result<String, EngineError> {
    digest(
        "ask-user-intent-v1",
        &(
            &config.run_id,
            &config.owner,
            &config.environment,
            &config.policy,
            body,
        ),
    )
}
pub(super) fn commit_question(
    tx: &rusqlite::Transaction<'_>,
    config: &RunConfig,
    invocation: &str,
    call_id: &str,
    body: &AskUser,
) -> Result<(), EngineError> {
    tx.execute("INSERT INTO runtime_questions(question_id,run_id,invocation,call_id,body,argument_digest,state) VALUES(?,?,?,?,?,?,'pending')", params![format!("question-{}",uuid::Uuid::new_v4()),config.run_id,invocation,call_id,encode(body)?,question_digest(config,body)?])?;
    tx.execute(
        "INSERT INTO events(run_id,kind) VALUES(?,'question.requested')",
        [&config.run_id],
    )?;
    Ok(())
}
impl Journal {
    fn validate_question_source(
        &self,
        config: &RunConfig,
        invocation: &str,
        call_id: &str,
        body: &AskUser,
    ) -> Result<(), EngineError> {
        let run = &config.run_id;
        let row:Option<(String,String)>=self.connection.query_row("SELECT p.body,p.digest FROM responses p JOIN runtime_invocations i USING(invocation) WHERE p.invocation=? AND p.run_id=? AND i.run_id=p.run_id AND i.state='committed' AND p.attempt=i.attempt AND i.attempt=1",params![invocation,run],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (encoded, expected) = row.ok_or(EngineError::RecoveryRequired)?;
        let response: ResponseCandidate =
            serde_json::from_str(&encoded).map_err(|_| EngineError::RecoveryRequired)?;
        if digest("model-response-v1", &response)? != expected
            || response.finish_reason != "tool_calls"
            || response.tool_calls.len() != 1
        {
            return Err(EngineError::RecoveryRequired);
        }
        let call = &response.tool_calls[0];
        if call.id != call_id
            || call.name != "ask_user"
            || AskUser::parse(&call.arguments).as_ref() != Some(body)
        {
            return Err(EngineError::RecoveryRequired);
        }
        let (catalog, pin) = self.checked_tool_pin(config, invocation, "ask_user")?;
        catalog
            .validate_input(&pin, &call.arguments)
            .map_err(|_| EngineError::RecoveryRequired)?;
        Ok(())
    }
    pub(super) fn pending_user_question(
        &self,
        stored: &StoredRun,
    ) -> Result<Option<PendingUserQuestion>, EngineError> {
        if !stored.user_questions {
            return Err(EngineError::Invalid);
        }
        let row:Option<(String,String,String,String,String)>=self.connection.query_row("SELECT question_id,body,argument_digest,invocation,call_id FROM runtime_questions WHERE run_id=? AND state='pending'",[&stored.config.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((question_id, body, argument_digest, invocation, call_id)) = row else {
            return Ok(None);
        };
        let body: AskUser = serde_json::from_str(&body).map_err(|_| EngineError::Invalid)?;
        if !body.validate() || question_digest(&stored.config, &body)? != argument_digest {
            return Err(EngineError::Invalid);
        }
        if stored.active.as_deref() != Some(invocation.as_str()) {
            return Err(EngineError::RecoveryRequired);
        }
        self.validate_question_source(&stored.config, &invocation, &call_id, &body)?;
        Ok(Some(PendingUserQuestion {
            question_id,
            argument_digest,
            questions: body.questions,
        }))
    }
    /// Accept exactly one current reply. Replays must match the original reply;
    /// this method never executes a model or inserts an approvals row.
    pub async fn answer_agent_question(
        &mut self,
        owner: &str,
        run: &str,
        answer: &AnswerUser,
        grant: &WorkspaceGrant,
        cancellation: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        self.answer_agent_question_authorized(
            owner,
            run,
            answer,
            AgentAuthority::read(grant),
            cancellation,
        )
        .await
    }
    pub async fn answer_agent_question_authorized(
        &mut self,
        owner: &str,
        run: &str,
        answer: &AnswerUser,
        authority: AgentAuthority<'_>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        let grant = authority.read;
        if !valid_id(&answer.question_id) || !valid_digest(&answer.argument_digest) {
            return Err(EngineError::Invalid);
        }
        let stored = self.stored_run(owner, run)?;
        self.check_memory_bindings(&stored, authority.memories)?;
        check_grant(&stored.config, grant)?;
        if !stored.user_questions {
            return Err(EngineError::Forbidden);
        }
        if *cancellation.borrow() || cancellation.has_changed().is_err() {
            return self.cancel_agent(owner, run, grant);
        }
        let row:Option<(String,String,String,String,String,Option<String>)>=self.connection.query_row("SELECT invocation,call_id,body,argument_digest,state,answer_digest FROM runtime_questions WHERE question_id=? AND run_id=?",params![answer.question_id,run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let (invocation, call_id, body, expected, state, prior) =
            row.ok_or(EngineError::Forbidden)?;
        let body: AskUser = serde_json::from_str(&body).map_err(|_| EngineError::Invalid)?;
        if !body.validate()
            || question_digest(&stored.config, &body)? != expected
            || answer.argument_digest != expected
        {
            return Err(JournalError::Conflict.into());
        }
        if !valid_answers(&answer.answers, body.questions.len()) {
            return Err(EngineError::Invalid);
        }
        self.validate_question_source(&stored.config, &invocation, &call_id, &body)?;
        let (catalog, pin) = self.checked_tool_pin(&stored.config, &invocation, "ask_user")?;
        catalog
            .validate_output(
                &pin,
                &serde_json::json!({"answers":answer.answers}),
                crate::catalog::OutputChannel::Model,
            )
            .map_err(|_| EngineError::RecoveryRequired)?;
        let answered = digest(
            "user-answer-v1",
            &(run, &answer.question_id, &expected, &answer.answers),
        )?;
        if state == "answered" {
            if prior.as_deref() != Some(answered.as_str()) {
                return Err(JournalError::Conflict.into());
            }
            return self
                .agent_view_with_memory(owner, run, grant, authority.memories)
                .await;
        }
        if state != "pending"
            || stored.phase != "awaiting_user"
            || stored.active.as_deref() != Some(invocation.as_str())
        {
            return Err(EngineError::RecoveryRequired);
        }
        let active: bool =
            self.connection
                .query_row("SELECT active FROM runs WHERE run_id=?", [run], |r| {
                    r.get(0)
                })?;
        if !active {
            return Err(EngineError::Forbidden);
        }
        if now()? >= stored.config.deadline_unix || stored.steps >= stored.config.max_steps {
            self.stop_agent(run, "limited", "answer_budget_or_deadline")?;
            return self.agent_view(owner, run);
        }
        let _leases = tokio::select! {
            _=cancellation.changed()=>return self.cancel_agent(owner,run,grant),
            result=tokio::time::timeout(Duration::from_secs(stored.config.deadline_unix.saturating_sub(now()?).max(0) as u64),self.revalidate_sources_authority(&stored.config,authority))=>result.map_err(|_|EngineError::RecoveryRequired)??,
        };
        if *cancellation.borrow() || grant.is_revoked() {
            return self.cancel_agent(owner, run, grant);
        }
        let mut messages = stored.messages.clone();
        messages.push(Message {
            role: Role::Tool,
            content: encode(&serde_json::json!({"answers":answer.answers}))?,
            tool_calls: vec![],
            tool_call_id: Some(call_id),
        });
        make_request_mode(
            &stored.config,
            "answer-validation",
            messages.clone(),
            true,
            stored.creation.is_some(),
            stored.editing.is_some(),
            MemoryToolSelection {
                bindings: &stored.memories,
                version: stored.memory_read_version,
            },
        )
        .validate()
        .map_err(|_| EngineError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_grant(&stored.config, grant)?;
        if now()? >= stored.config.deadline_unix {
            return Err(EngineError::RecoveryRequired);
        }
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_runs rr JOIN runs r USING(run_id) JOIN runtime_invocations i ON i.invocation=rr.active_invocation JOIN responses p ON p.invocation=i.invocation WHERE rr.run_id=? AND r.owner=? AND r.active=1 AND rr.phase='awaiting_user' AND i.invocation=? AND i.run_id=rr.run_id AND p.run_id=rr.run_id AND p.attempt=i.attempt AND i.state='committed')",params![run,owner,invocation],|r|r.get(0))?;
        if !valid {
            return Err(EngineError::RecoveryRequired);
        }
        if tx.execute("UPDATE runtime_questions SET state='answered',answers=?,answer_digest=? WHERE question_id=? AND run_id=? AND state='pending'",params![encode(&answer.answers)?,answered,answer.question_id,run])?!=1 {
            return Err(EngineError::RecoveryRequired);
        }
        tx.execute("UPDATE runtime_runs SET phase='ready',messages=?,steps=steps+1,active_invocation=NULL WHERE run_id=?",params![encode(&messages)?,run])?;
        tx.execute(
            "INSERT INTO events(run_id,kind) VALUES(?,'question.answered')",
            [run],
        )?;
        crate::memory::check_execution_grants(
            &tx,
            authority.memories,
            owner,
            &stored.config.policy,
            &self.memory_session,
        )
        .map_err(memory::error)?;
        tx.commit()?;
        self.agent_view(owner, run)
    }
}
