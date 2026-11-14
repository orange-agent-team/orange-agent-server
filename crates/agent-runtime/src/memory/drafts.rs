use super::*;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMutation {
    Put,
    Delete,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRequest {
    pub entry_id: String,
    pub expected_version: Option<String>,
    pub source_run: String,
    pub origin: MemoryOrigin,
    pub mutation: MemoryMutation,
    pub payload: Option<MemoryPayload>,
}
impl MemoryWriteRequest {
    fn validate(&self) -> Result<(), MemoryError> {
        if !valid_id(&self.entry_id) || !valid_id(&self.source_run) {
            return Err(MemoryError::Invalid);
        }
        if let Some(v) = &self.expected_version {
            version(v)?;
        }
        match (&self.mutation, &self.payload) {
            (MemoryMutation::Put, Some(v)) => v.validate(),
            (MemoryMutation::Delete, None) if self.expected_version.is_some() => Ok(()),
            _ => Err(MemoryError::Invalid),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    scope: String,
    actor: String,
    scope_revision: i64,
    policy: String,
    entry_id: String,
    expected_version: Option<String>,
    source_run: String,
    origin: MemoryOrigin,
    mutation: MemoryMutation,
    payload_digest: Option<String>,
    expires_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryReview {
    pub draft_id: String,
    pub scope_id: String,
    pub entry_id: String,
    pub expected_version: Option<String>,
    pub source_actor: String,
    pub source_run: String,
    pub origin: MemoryOrigin,
    pub mutation: MemoryMutation,
    pub payload: Option<MemoryPayload>,
    pub digest: String,
    pub expires_at: i64,
}
pub struct PreparedMemoryDraft {
    grant: MemoryGrant,
    record: Draft,
    review: MemoryReview,
    #[cfg(test)]
    before_commit: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl PreparedMemoryDraft {
    #[cfg(test)]
    pub(crate) fn before_commit_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_commit.lock().expect("test hook") = Some(Box::new(hook));
    }

    pub fn view<'a>(&'a self, journal: &'a Journal) -> Result<&'a MemoryReview, MemoryError> {
        validate(
            &journal.connection,
            &self.grant,
            &self.record,
            &journal.memory_session,
        )?;
        let same:bool=journal.connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_drafts WHERE draft_id=? AND scope_id=? AND actor=? AND state='pending' AND digest=?)",params![self.review.draft_id,self.record.scope,self.record.actor,self.review.digest],|r|r.get(0))?;
        if !same {
            return Err(MemoryError::Conflict);
        }
        Ok(&self.review)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryDraftOutcome {
    pub draft_id: String,
    pub scope_id: String,
    pub entry_id: String,
    pub digest: String,
    pub state: String,
    pub result_version: Option<String>,
}
fn validate(
    db: &Connection,
    grant: &MemoryGrant,
    record: &Draft,
    session: &str,
) -> Result<(), MemoryError> {
    check(db, grant, true, session)?;
    if record.scope != grant.scope
        || record.actor != grant.actor
        || record.scope_revision != grant.revision
        || record.policy != grant.policy
    {
        return Err(MemoryError::Denied);
    }
    if record.expires_at <= now()? {
        return Err(MemoryError::ApprovalRequired);
    }
    let active:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE run_id=? AND owner=? AND policy=? AND active=1) AND NOT EXISTS(SELECT 1 FROM memory_source_blocks WHERE scope_id=? AND run_id=?)",params![record.source_run,grant.actor,grant.policy,grant.scope,record.source_run],|r|r.get(0))?;
    if !active {
        return Err(MemoryError::Denied);
    }
    let old: Option<(i64, String)> = db
        .query_row(
            "SELECT version,state FROM memory_entries WHERE scope_id=? AND entry_id=?",
            params![grant.scope, record.entry_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match (old, record.expected_version.as_deref()) {
        (None, None) if record.mutation == MemoryMutation::Put => Ok(()),
        (Some((v, state)), Some(expected)) if state != "deleted" && version(expected)? == v => {
            Ok(())
        }
        _ => Err(MemoryError::Conflict),
    }
}
impl Journal {
    /// Trusted Host/model adapter creates a draft, never a learned fact. The
    /// model adapter must force ModelInference rather than trusting model origin.
    pub fn prepare_memory_write(
        &mut self,
        grant: &MemoryGrant,
        key: &str,
        request: MemoryWriteRequest,
    ) -> Result<PreparedMemoryDraft, MemoryError> {
        if !valid_id(key) {
            return Err(MemoryError::Invalid);
        }
        request.validate()?;
        check(&self.connection, grant, true, &self.memory_session)?;
        let mut record = Draft {
            scope: grant.scope.clone(),
            actor: grant.actor.clone(),
            scope_revision: grant.revision,
            policy: grant.policy.clone(),
            entry_id: request.entry_id.clone(),
            expected_version: request.expected_version.clone(),
            source_run: request.source_run.clone(),
            origin: request.origin,
            mutation: request.mutation,
            payload_digest: request
                .payload
                .as_ref()
                .map(|v| digest("memory-payload.v1", v))
                .transpose()?,
            expires_at: now()?.checked_add(300).ok_or(MemoryError::Invalid)?,
        };
        validate(&self.connection, grant, &record, &self.memory_session)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Expired payloads are scrubbed; hashes and source metadata remain auditable.
        tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE scope_id=? AND state='pending' AND CAST(json_extract(body,'$.expires_at') AS INTEGER)<=?",params![grant.scope,now()?])?;
        type Prior = (String, String, String, String, Option<String>);
        let prior:Option<Prior>=tx.query_row("SELECT draft_id,body,digest,state,payload FROM memory_drafts WHERE scope_id=? AND actor=? AND request_key=?",params![grant.scope,grant.actor,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let id = if let Some((id, body, stored, state, payload)) = prior {
            if state != "pending" || body.len() > 8192 {
                return Err(MemoryError::Conflict);
            }
            let prior: Draft = serde_json::from_str(&body).map_err(|_| MemoryError::Invalid)?;
            record.expires_at = prior.expires_at;
            if digest("memory-draft.v1", &prior)? != stored
                || digest("memory-draft.v1", &record)? != stored
                || payload != request.payload.as_ref().map(encoded).transpose()?
            {
                return Err(MemoryError::Conflict);
            }
            id
        } else {
            let (total,pending):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(state='pending'),0) FROM memory_drafts WHERE scope_id=?",[&grant.scope],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if total >= 10000 || pending >= 128 {
                return Err(MemoryError::Limit);
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO memory_drafts(draft_id,scope_id,entry_id,actor,request_key,body,digest,payload,state) VALUES(?,?,?,?,?,?,?,?,'pending')",params![id,grant.scope,record.entry_id,grant.actor,key,encoded(&record)?,digest("memory-draft.v1",&record)?,request.payload.as_ref().map(encoded).transpose()?])?;
            tx.execute("INSERT INTO memory_audit(scope_id,actor,action,entry_id,revision,digest) VALUES(?,?,'draft_prepared',?,?,?)",params![grant.scope,grant.actor,record.entry_id,grant.revision,digest("memory-draft.v1",&record)?])?;
            id
        };
        validate(&tx, grant, &record, &self.memory_session)?;
        tx.commit()?;
        let review = MemoryReview {
            draft_id: id,
            scope_id: grant.scope.clone(),
            entry_id: record.entry_id.clone(),
            expected_version: record.expected_version.clone(),
            source_actor: grant.actor.clone(),
            source_run: record.source_run.clone(),
            origin: record.origin,
            mutation: record.mutation,
            payload: request.payload,
            digest: digest("memory-draft.v1", &record)?,
            expires_at: record.expires_at,
        };
        Ok(PreparedMemoryDraft {
            grant: grant.clone(),
            record,
            review,
            #[cfg(test)]
            before_commit: std::sync::Mutex::new(None),
        })
    }
    pub fn approve_memory_write(
        &mut self,
        prepared: PreparedMemoryDraft,
        expected_digest: &str,
    ) -> Result<MemoryDraftOutcome, MemoryError> {
        let bound: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_memory_operations WHERE draft_id=?)",
            [&prepared.review.draft_id],
            |r| r.get(0),
        )?;
        if bound {
            return Err(MemoryError::ApprovalRequired);
        }
        self.approve_memory_write_checked(prepared, expected_digest, |_| Ok(()))
    }
    pub(crate) fn approve_memory_write_checked(
        &mut self,
        prepared: PreparedMemoryDraft,
        expected_digest: &str,
        additional_check: impl Fn(&Connection) -> Result<(), MemoryError>,
    ) -> Result<MemoryDraftOutcome, MemoryError> {
        prepared.view(self)?;
        let record = &prepared.record;
        let review = &prepared.review;
        let grant = &prepared.grant;
        if review.digest != expected_digest || digest("memory-draft.v1", record)? != expected_digest
        {
            return Err(MemoryError::ApprovalRequired);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate(&tx, grant, record, &self.memory_session)?;
        additional_check(&tx)?;
        let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_drafts WHERE draft_id=? AND body=? AND digest=? AND state='pending' AND payload IS ?)",params![review.draft_id,encoded(record)?,expected_digest,review.payload.as_ref().map(encoded).transpose()?],|r|r.get(0))?;
        if !live {
            return Err(MemoryError::Conflict);
        }
        let next = record
            .expected_version
            .as_deref()
            .map(version)
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(MemoryError::Limit)?;
        match record.mutation {
            MemoryMutation::Put => {
                let payload = review.payload.as_ref().ok_or(MemoryError::Invalid)?;
                payload.validate()?;
                let encoded = encoded(payload)?;
                let sha = digest("memory-payload.v1", payload)?;
                if record.payload_digest.as_deref() != Some(&sha) {
                    return Err(MemoryError::Invalid);
                }
                let(count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(payload AS BLOB))),0) FROM memory_entries WHERE scope_id=? AND entry_id!=?",params![grant.scope,record.entry_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                if count >= 1000
                    || bytes
                        .checked_add(encoded.len() as i64)
                        .ok_or(MemoryError::Limit)?
                        > 4 * 1024 * 1024
                {
                    return Err(MemoryError::Limit);
                }
                let state = if payload.enabled {
                    "active"
                } else {
                    "disabled"
                };
                if record.expected_version.is_none(){tx.execute("INSERT INTO memory_entries VALUES(?,?,?,?,?,?,?,?,?)",params![grant.scope,record.entry_id,next,state,encoded,sha,grant.actor,record.source_run,record.origin.name()])?;}
    else if tx.execute("UPDATE memory_entries SET version=?,state=?,payload=?,payload_digest=?,source_actor=?,source_run=?,origin=? WHERE scope_id=? AND entry_id=? AND version=? AND state!='deleted'",params![next,state,encoded,sha,grant.actor,record.source_run,record.origin.name(),grant.scope,record.entry_id,next-1])?!=1{return Err(MemoryError::Conflict);}
            }
            MemoryMutation::Delete => {
                // Fence every contributing Run, not only the most recent edit. This also
                // invalidates pending learning to a different key from those old Runs.
                tx.execute("INSERT OR IGNORE INTO memory_source_blocks SELECT scope_id,json_extract(body,'$.source_run') FROM memory_drafts WHERE scope_id=? AND entry_id=?",params![grant.scope,record.entry_id])?;
                tx.execute("INSERT OR IGNORE INTO memory_source_blocks SELECT scope_id,source_run FROM memory_entries WHERE scope_id=? AND entry_id=?",params![grant.scope,record.entry_id])?;
                if tx.execute("UPDATE memory_entries SET version=?,state='deleted',payload=NULL,payload_digest=?,source_actor=?,source_run=?,origin=? WHERE scope_id=? AND entry_id=? AND version=? AND state!='deleted'",params![next,digest("memory-deletion.v1",&(grant.scope.as_str(),record.entry_id.as_str(),next))?,grant.actor,record.source_run,record.origin.name(),grant.scope,record.entry_id,next-1])?!=1{return Err(MemoryError::Conflict);}
            }
        }
        if tx.execute("UPDATE memory_drafts SET state='approved',payload=NULL,result_version=? WHERE draft_id=? AND state='pending'",params![next,review.draft_id])?!=1{return Err(MemoryError::Conflict);}
        tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE scope_id=? AND state='pending' AND (entry_id=? OR json_extract(body,'$.source_run') IN(SELECT run_id FROM memory_source_blocks WHERE scope_id=?))",params![grant.scope,record.entry_id,grant.scope])?;
        tx.execute("INSERT INTO memory_audit(scope_id,actor,action,entry_id,revision,digest) VALUES(?,?,?, ?,?,?)",params![grant.scope,grant.actor,if record.mutation==MemoryMutation::Delete{"entry_deleted"}else{"entry_updated"},record.entry_id,next,expected_digest])?;
        #[cfg(test)]
        if let Some(hook) = prepared.before_commit.lock().expect("test hook").take() {
            hook();
        }
        // Delete fences its own source Run, so revalidate authority rather than the
        // now intentionally invalidated proposal source at the commit boundary.
        check(&tx, grant, true, &self.memory_session)?;
        additional_check(&tx)?;
        tx.commit()?;
        self.memory_draft_outcome(grant, &review.draft_id)?
            .ok_or(MemoryError::Conflict)
    }
    pub fn memory_draft_outcome(
        &self,
        grant: &MemoryGrant,
        id: &str,
    ) -> Result<Option<MemoryDraftOutcome>, MemoryError> {
        check(&self.connection, grant, false, &self.memory_session)?;
        self.owned_memory_draft_metadata(&grant.actor, &grant.scope, id)
    }
    pub(crate) fn owned_memory_draft_metadata(
        &self,
        actor: &str,
        scope: &str,
        id: &str,
    ) -> Result<Option<MemoryDraftOutcome>, MemoryError> {
        if ![actor, scope, id].iter().all(|v| valid_id(v)) {
            return Err(MemoryError::Invalid);
        }
        type Row = (String, String, String, String, Option<i64>);
        let row:Option<Row>=self.connection.query_row("SELECT entry_id,body,digest,state,result_version FROM memory_drafts WHERE draft_id=? AND scope_id=? AND actor=?",params![id,scope,actor],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((entry, body, sha, state, result)) = row else {
            return Ok(None);
        };
        if body.len() > 8192 {
            return Err(MemoryError::Limit);
        }
        let record: Draft = serde_json::from_str(&body).map_err(|_| MemoryError::Invalid)?;
        if record.actor != actor
            || record.scope != scope
            || record.entry_id != entry
            || digest("memory-draft.v1", &record)? != sha
        {
            return Err(MemoryError::Invalid);
        }
        let expected_result = record
            .expected_version
            .as_deref()
            .map(version)
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(MemoryError::Invalid)?;
        if !valid_id(&record.entry_id)
            || !valid_id(&record.source_run)
            || !valid_id(&record.policy)
            || record.scope_revision < 1
            || record.expires_at < 1
            || match state.as_str() {
                "approved" => result != Some(expected_result),
                "pending" | "invalidated" => result.is_some(),
                _ => true,
            }
        {
            return Err(MemoryError::Invalid);
        }
        Ok(Some(MemoryDraftOutcome {
            draft_id: id.into(),
            scope_id: scope.to_owned(),
            entry_id: entry,
            digest: sha,
            state,
            result_version: result.map(|n| n.to_string()),
        }))
    }
    pub fn memory_draft_by_key(
        &self,
        grant: &MemoryGrant,
        key: &str,
    ) -> Result<Option<MemoryDraftOutcome>, MemoryError> {
        if !valid_id(key) {
            return Err(MemoryError::Invalid);
        }
        check(&self.connection, grant, false, &self.memory_session)?;
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT draft_id FROM memory_drafts WHERE scope_id=? AND actor=? AND request_key=?",
                params![grant.scope, grant.actor, key],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| {
            self.memory_draft_outcome(grant, &id)?
                .ok_or(MemoryError::Conflict)
        })
        .transpose()
    }
}

impl Journal {
    /// Fresh management review of a persisted draft; does not restore an old
    /// process grant, grant approval, refresh expiry or change the original hash.
    pub fn review_memory_draft(
        &self,
        grant: &MemoryGrant,
        id: &str,
    ) -> Result<PreparedMemoryDraft, MemoryError> {
        if !valid_id(id) {
            return Err(MemoryError::Invalid);
        }
        check(&self.connection, grant, true, &self.memory_session)?;
        type Row = (String, String, Option<String>, String);
        let (body,expected,payload,state):Row=self.connection.query_row("SELECT body,digest,payload,state FROM memory_drafts WHERE draft_id=? AND scope_id=? AND actor=?",params![id,grant.scope,grant.actor],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or(MemoryError::Denied)?;
        if state != "pending" {
            return Err(MemoryError::Conflict);
        }
        if body.len() > 8192 || payload.as_ref().is_some_and(|v| v.len() > MAX_PAYLOAD) {
            return Err(MemoryError::Limit);
        }
        let record: Draft = serde_json::from_str(&body).map_err(|_| MemoryError::Invalid)?;
        let payload = payload
            .map(|v| serde_json::from_str::<MemoryPayload>(&v).map_err(|_| MemoryError::Invalid))
            .transpose()?;
        if digest("memory-draft.v1", &record)? != expected
            || payload
                .as_ref()
                .map(|v| digest("memory-payload.v1", v))
                .transpose()?
                != record.payload_digest
        {
            return Err(MemoryError::Invalid);
        }
        MemoryWriteRequest {
            entry_id: record.entry_id.clone(),
            expected_version: record.expected_version.clone(),
            source_run: record.source_run.clone(),
            origin: record.origin,
            mutation: record.mutation,
            payload: payload.clone(),
        }
        .validate()?;
        validate(&self.connection, grant, &record, &self.memory_session)?;
        let review = MemoryReview {
            draft_id: id.into(),
            scope_id: grant.scope.clone(),
            entry_id: record.entry_id.clone(),
            expected_version: record.expected_version.clone(),
            source_actor: grant.actor.clone(),
            source_run: record.source_run.clone(),
            origin: record.origin,
            mutation: record.mutation,
            payload,
            digest: expected,
            expires_at: record.expires_at,
        };
        let prepared = PreparedMemoryDraft {
            grant: grant.clone(),
            record,
            review,
            #[cfg(test)]
            before_commit: std::sync::Mutex::new(None),
        };
        prepared.view(self)?;
        Ok(prepared)
    }
    /// Rejecting a proposal scrubs its payload, never edits an approved fact.
    pub fn discard_memory_draft(
        &mut self,
        grant: &MemoryGrant,
        id: &str,
        expected: &str,
    ) -> Result<MemoryDraftOutcome, MemoryError> {
        check(&self.connection, grant, true, &self.memory_session)?;
        let outcome = self
            .memory_draft_outcome(grant, id)?
            .ok_or(MemoryError::Denied)?;
        if outcome.digest != expected {
            return Err(MemoryError::ApprovalRequired);
        }
        if outcome.state == "invalidated" {
            return Ok(outcome);
        }
        if outcome.state != "pending" {
            return Err(MemoryError::Conflict);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE draft_id=? AND scope_id=? AND actor=? AND digest=? AND state='pending'",params![id,grant.scope,grant.actor,expected])?!=1{return Err(MemoryError::Conflict);}
        tx.execute("INSERT INTO memory_audit(scope_id,actor,action,entry_id,revision,digest) VALUES(?,?,'draft_discarded',?,?,?)",params![grant.scope,grant.actor,outcome.entry_id,grant.revision,expected])?;
        check(&tx, grant, true, &self.memory_session)?;
        tx.commit()?;
        self.memory_draft_outcome(grant, id)?
            .ok_or(MemoryError::Conflict)
    }
}
