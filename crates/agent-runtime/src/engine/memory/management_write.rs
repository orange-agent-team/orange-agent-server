//! Human management edits verify any prior body again at preview and commit.
use super::*;
use crate::memory::{MemoryOrigin, MemoryValue, MemoryWriteRequest};
#[derive(Debug, Clone, Serialize)]
pub struct ManagedMemoryReview {
    pub review: MemoryReview,
    /// Binds the proposed mutation and the exact prior entry snapshot shown.
    pub approval_digest: String,
    pub prior: Option<MemoryValue>,
}
impl Journal {
    /// Content-free lost-reply recovery; never restores execution or read access.
    pub fn user_memory_write_metadata(
        &self,
        actor: &str,
        scope: &str,
        draft: &str,
    ) -> Result<Option<MemoryDraftOutcome>, EngineError> {
        let value = self
            .owned_memory_draft_metadata(actor, scope, draft)
            .map_err(error)?;
        if value.is_some() {
            let allowed:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_drafts d WHERE draft_id=? AND actor=? AND scope_id=? AND json_extract(body,'$.origin')='user_declaration' AND NOT EXISTS(SELECT 1 FROM runtime_memory_operations m WHERE m.draft_id=d.draft_id))",params![draft,actor,scope],|r|r.get(0))?;
            if !allowed {
                return Err(EngineError::Forbidden);
            }
        }
        Ok(value)
    }
    pub fn user_memory_write_by_key(
        &self,
        actor: &str,
        scope: &str,
        key: &str,
    ) -> Result<Option<MemoryDraftOutcome>, EngineError> {
        if ![actor, scope, key].iter().all(|v| valid_id(v)) {
            return Err(EngineError::Invalid);
        }
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT draft_id FROM memory_drafts WHERE actor=? AND scope_id=? AND request_key=?",
                params![actor, scope, key],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.user_memory_write_metadata(actor, scope, &id),
            None => Ok(None),
        }
    }
    /// Recover pending user requests after Host restart, even when the original
    /// data grant is gone. Only the author can discover their draft metadata.
    pub fn pending_user_memory_drafts(
        &self,
        actor: &str,
        scope: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<MemoryDraftOutcome>, EngineError> {
        if !valid_id(actor)
            || !valid_id(scope)
            || after.is_some_and(|id| !valid_id(id))
            || !(1..=100).contains(&limit)
        {
            return Err(EngineError::Invalid);
        }
        let mut query=self.connection.prepare(
            "SELECT d.draft_id FROM memory_drafts d
             WHERE d.actor=?1 AND d.scope_id=?2 AND d.state='pending'
               AND (?3 IS NULL OR d.draft_id>?3) AND json_extract(d.body,'$.origin')='user_declaration'
               AND NOT EXISTS(SELECT 1 FROM runtime_memory_operations m WHERE m.draft_id=d.draft_id)
             ORDER BY d.draft_id LIMIT ?4")?;
        let ids = query.query_map(params![actor, scope, after, limit], |row| {
            row.get::<_, String>(0)
        })?;
        ids.map(|id| {
            self.user_memory_write_metadata(actor, scope, &id?)?
                .ok_or(EngineError::RecoveryRequired)
        })
        .collect()
    }

    /// Invalidate exactly the author's reviewed draft and erase its pending
    /// payload in the same transaction as its audit event. Repeated discard
    /// returns the existing invalidated receipt.
    pub fn discard_user_memory_draft(
        &self,
        actor: &str,
        scope: &str,
        draft: &str,
        expected_digest: &str,
    ) -> Result<MemoryDraftOutcome, EngineError> {
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut outcome = self
            .user_memory_write_metadata(actor, scope, draft)?
            .ok_or(EngineError::Forbidden)?;
        if outcome.digest != expected_digest {
            return Err(EngineError::ApprovalRequired);
        }
        match outcome.state.as_str() {
            "invalidated" => {}
            "pending" => {
                if tx.execute("UPDATE memory_drafts SET state='invalidated',payload=NULL WHERE draft_id=? AND state='pending' AND digest=?",params![draft,expected_digest])?!=1 {return Err(EngineError::RecoveryRequired);}
                tx.execute("INSERT INTO memory_audit(scope_id,actor,action,entry_id,revision,digest) SELECT scope_id,?,'user_draft_discarded',?,revision,? FROM memory_scopes WHERE scope_id=?",params![actor,outcome.entry_id,expected_digest,scope])?;
                outcome.state = "invalidated".into();
            }
            _ => return Err(EngineError::RecoveryRequired),
        }
        tx.commit()?;
        Ok(outcome)
    }
    async fn management_prior(
        &self,
        authority: MemoryManagementAuthority<'_>,
        entry: &str,
        version: Option<&str>,
    ) -> Result<Option<VerifiedMemoryRead>, EngineError> {
        match version {
            Some(version) => self
                .read_memory_verified(
                    authority,
                    MemoryReadSelection {
                        entry_id: entry.into(),
                        expected_version: Some(version.into()),
                        start_byte: 0,
                        max_bytes: 4096,
                        include_disabled: true,
                    },
                )
                .await
                .map(Some),
            None => Ok(None),
        }
    }
    fn managed_review(
        &self,
        review: MemoryReview,
        prior: Option<&VerifiedMemoryRead>,
    ) -> Result<ManagedMemoryReview, EngineError> {
        if review.origin != MemoryOrigin::UserDeclaration {
            return Err(EngineError::Forbidden);
        }
        let prior = prior.map(|p| p.view(self).cloned()).transpose()?;
        let approval_digest = digest("memory-management-approval.v1", &(&review.digest, &prior))?;
        Ok(ManagedMemoryReview {
            review,
            approval_digest,
            prior,
        })
    }
    /// Authenticated Host supplies a real human activity source Run. A model
    /// inference cannot be relabelled through this endpoint.
    pub async fn prepare_user_memory_write(
        &mut self,
        authority: MemoryManagementAuthority<'_>,
        request_key: &str,
        request: MemoryWriteRequest,
    ) -> Result<ManagedMemoryReview, EngineError> {
        if request.origin != MemoryOrigin::UserDeclaration {
            return Err(EngineError::Forbidden);
        }
        let prior = self
            .management_prior(
                authority,
                &request.entry_id,
                request.expected_version.as_deref(),
            )
            .await?;
        let prepared = self
            .prepare_memory_write(authority.grant, request_key, request)
            .map_err(error)?;
        self.managed_review(prepared.view(self).map_err(error)?.clone(), prior.as_ref())
    }
    pub async fn review_user_memory_write(
        &self,
        authority: MemoryManagementAuthority<'_>,
        draft: &str,
    ) -> Result<ManagedMemoryReview, EngineError> {
        let prepared = self
            .review_memory_draft(authority.grant, draft)
            .map_err(error)?;
        let review = prepared.view(self).map_err(error)?.clone();
        let prior = self
            .management_prior(
                authority,
                &review.entry_id,
                review.expected_version.as_deref(),
            )
            .await?;
        self.managed_review(review, prior.as_ref())
    }
    /// Reacquire original source proof; do not hold a preview's leases while a
    /// human considers it. The final proof stays alive through the fact commit.
    pub async fn approve_user_memory_write(
        &mut self,
        authority: MemoryManagementAuthority<'_>,
        draft: &str,
        expected: &str,
    ) -> Result<MemoryDraftOutcome, EngineError> {
        let prepared = self
            .review_memory_draft(authority.grant, draft)
            .map_err(error)?;
        let review = prepared.view(self).map_err(error)?.clone();
        let prior = self
            .management_prior(
                authority,
                &review.entry_id,
                review.expected_version.as_deref(),
            )
            .await?;
        let visible = self.managed_review(review, prior.as_ref())?;
        if visible.approval_digest != expected {
            return Err(EngineError::ApprovalRequired);
        }
        let changed = visible
            .review
            .expected_version
            .as_deref()
            .map(|v| v.parse::<i64>().map_err(|_| EngineError::Invalid))
            .transpose()?
            .map(|n| n.checked_add(1).ok_or(EngineError::Invalid))
            .transpose()?;
        let session = self.memory_session.clone();
        self.approve_memory_write_checked(prepared, &visible.review.digest, |db| {
            if let Some(prior) = &prior {
                prior
                    .check_after_db(
                        db,
                        &session,
                        changed.map(|n| {
                            (
                                visible.review.scope_id.as_str(),
                                visible.review.entry_id.as_str(),
                                n,
                            )
                        }),
                    )
                    .map_err(|_| MemoryError::Denied)?;
            }
            Ok(())
        })
        .map_err(error)
    }
}
