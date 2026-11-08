//! Persistent exact preview bound to one complete committed model tool call.
use super::*;
use orange_protocol::model::ResponseCandidate;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreationPreview {
    pub operation_id: String,
    pub call_id: String,
    pub arguments: CreateFileArgs,
    pub argument_digest: String,
    pub environment: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    tool: String,
    arguments: CreateFileArgs,
}
impl Journal {
    fn creation_source(
        &self,
        op: &Operation<'_>,
        args: &CreateFileArgs,
        call_id: &str,
    ) -> Result<(), CreateFileError> {
        if !valid_id(call_id) {
            return Err(JournalError::Invalid.into());
        }
        let row:Option<(String,String)>=self.connection.query_row("SELECT body,digest FROM responses WHERE invocation=? AND run_id=? AND attempt=1 AND body IS NOT NULL",params![op.invocation,op.run],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        let (body, digest) = row.ok_or(JournalError::ResponseRequired)?;
        let response: ResponseCandidate =
            serde_json::from_str(&body).map_err(|_| JournalError::ResponseRequired)?;
        if request_digest("model-response-v1", &response).map_err(|_| JournalError::Invalid)?
            != digest
            || response.finish_reason != "tool_calls"
            || !valid_id(&response.response_id)
            || response.tool_calls.is_empty()
            || response.tool_calls.len() > 80
        {
            return Err(JournalError::ResponseRequired.into());
        }
        let mut ids = std::collections::BTreeSet::new();
        if response
            .tool_calls
            .iter()
            .any(|c| !valid_id(&c.id) || !ids.insert(c.id.clone()))
        {
            return Err(JournalError::ResponseRequired.into());
        }
        let call = response
            .tool_calls
            .iter()
            .find(|c| c.id == call_id)
            .ok_or(JournalError::ResponseRequired)?;
        let actual: CreateFileArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|_| JournalError::ResponseRequired)?;
        if call.name != "write_file" || actual != *args || actual.digest()? != op.argument_digest {
            return Err(JournalError::Conflict.into());
        }
        Ok(())
    }
    /// Persist the concrete preview before approval. Generic digest-only
    /// operations cannot execute a creation until this binding exists.
    pub fn prepare_file_creation(
        &mut self,
        grant: &CreateFileGrant,
        op: &Operation<'_>,
        args: &CreateFileArgs,
        call_id: &str,
    ) -> Result<CreationPreview, CreateFileError> {
        validate_binding(grant, op, args)?;
        self.validate_creation_consent(grant)?;
        self.creation_source(op, args, call_id)?;
        self.prepare(op)?;
        let encoded = serde_json::to_string(&Request {
            tool: "write_file".into(),
            arguments: args.clone(),
        })
        .map_err(|_| JournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(JournalError::from)?;
        let row: (Option<String>, Option<String>, String) = tx
            .query_row(
                "SELECT request_json,call_id,state FROM operations WHERE operation_id=?",
                [op.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(JournalError::from)?;
        match row {
            (None, None, state) if state == "intended" => {
                tx.execute("UPDATE operations SET request_json=?,call_id=? WHERE operation_id=? AND state='intended' AND request_json IS NULL AND call_id IS NULL",params![encoded,call_id,op.id]).map_err(JournalError::from)?;
                tx.execute("INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.preview_bound')",params![op.run,op.id]).map_err(JournalError::from)?;
            }
            (Some(body), Some(id), _) if body == encoded && id == call_id => {}
            _ => return Err(JournalError::Conflict.into()),
        }
        tx.commit().map_err(JournalError::from)?;
        self.file_creation_preview(grant, op)
    }
    /// The trusted UI displays this exact stored body, never a mutable duplicate
    /// supplied by the model or by a resumed caller.
    pub fn file_creation_preview(
        &self,
        grant: &CreateFileGrant,
        op: &Operation<'_>,
    ) -> Result<CreationPreview, CreateFileError> {
        let row:Option<(String,String)>=self.connection.query_row("SELECT request_json,call_id FROM operations o JOIN runs r USING(run_id) WHERE o.operation_id=? AND o.owner=? AND o.run_id=? AND o.invocation=? AND o.argument_digest=? AND o.environment=? AND o.policy=? AND o.tool_version='write_file.v1' AND o.requires_approval=1 AND r.owner=o.owner AND r.policy=o.policy AND r.active=1 AND request_json IS NOT NULL AND call_id IS NOT NULL",params![op.id,op.owner,op.run,op.invocation,op.argument_digest,op.environment,op.policy_version],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(JournalError::from)?;
        let (encoded, call_id) = row.ok_or(JournalError::ResponseRequired)?;
        let request: Request = serde_json::from_str(&encoded).map_err(|_| JournalError::Invalid)?;
        if request.tool != "write_file" {
            return Err(JournalError::Invalid.into());
        }
        validate_binding(grant, op, &request.arguments)?;
        self.validate_creation_consent(grant)?;
        self.creation_source(op, &request.arguments, &call_id)?;
        Ok(CreationPreview {
            operation_id: op.id.into(),
            call_id,
            arguments: request.arguments,
            argument_digest: op.argument_digest.into(),
            environment: op.environment.into(),
        })
    }
    pub fn approve_file_creation(
        &self,
        grant: &CreateFileGrant,
        op: &Operation<'_>,
        expected_digest: &str,
        actor: &str,
        expires_at: i64,
    ) -> Result<(), CreateFileError> {
        let preview = self.file_creation_preview(grant, op)?;
        if preview.argument_digest != expected_digest {
            return Err(JournalError::Conflict.into());
        }
        self.approve(op, actor, expires_at)?;
        Ok(())
    }
}
pub(super) fn validate_binding(
    grant: &CreateFileGrant,
    op: &Operation<'_>,
    args: &CreateFileArgs,
) -> Result<(), CreateFileError> {
    args.validate()?;
    if grant.grant.is_revoked() {
        return Err(WorkspaceError::Revoked.into());
    }
    if !op.requires_approval
        || op.tool_version != "write_file.v1"
        || op.argument_digest != args.digest()?
        || op.environment != grant.environment
        || op.owner != grant.grant.owner()
        || op.policy_version != grant.grant.policy()
    {
        return Err(WorkspaceError::Forbidden.into());
    }
    Ok(())
}
