use super::*;
use orange_protocol::{InvocationPurpose, Money, SubmitJob};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Confirmation {
    quote_id: String,
    request_projection_digest: String,
    max_amount: Money,
}
impl ArtifactStore {
    pub(super) async fn cloud_status(
        &self,
        actor: &ArtifactActor,
        run: &str,
    ) -> Result<Value, ArtifactError> {
        if !valid_id(run) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let owner: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1 AND active")
                .bind(&actor.user)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ArtifactError::Forbidden)?;
        authority::scope(&mut tx, actor, &owner, false).await?;
        let row=sqlx::query("SELECT r.status,r.root_budget_id,r.input_version_id,p.model_version,p.price_version,p.invocation_id,p.output_limit,cap.execution_digest AS tool_digest,revoked.receipt_id AS revoked_directory,
            m.request_projection_digest,j.job_id,f.commit_id,tool.run_id AS tool_response,op.state AS tool_state,a.state AS result_state,src.state AS source_state
            FROM cloud_runs r JOIN cloud_model_plans p USING(run_id,owner_id) JOIN artifacts src ON src.artifact_id=r.input_artifact_id
            LEFT JOIN cloud_directory_capabilities cap USING(run_id) LEFT JOIN cloud_directory_revocations revoked USING(run_id) LEFT JOIN cloud_model_proposals m USING(run_id) LEFT JOIN cloud_model_jobs j USING(run_id)
            LEFT JOIN cloud_tool_responses tool ON tool.run_id=r.run_id LEFT JOIN cloud_operation_intents intent ON intent.run_id=r.run_id
            LEFT JOIN cloud_operation_state op ON op.operation_id=intent.operation_id LEFT JOIN cloud_model_results f ON f.run_id=r.run_id LEFT JOIN artifacts a ON a.artifact_id=f.artifact_id AND a.version_id=f.version_id
            WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3")
            .bind(run).bind(&owner).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        let job: Option<String> = row.try_get("job_id")?;
        let proposal: Option<String> = row.try_get("request_projection_digest")?;
        let stage = if row.try_get::<String, _>("source_state")? != "available" {
            "unavailable"
        } else {
            match row.try_get::<String, _>("status")?.as_str() {
                "completed"
                    if row.try_get::<Option<String>, _>("result_state")?.as_deref()
                        == Some("available") =>
                {
                    "result_available"
                }
                "waiting_approval"
                    if row.try_get::<Option<String>, _>("tool_response")?.is_some() =>
                {
                    "waiting_tool"
                }
                "paused"
                    if row.try_get::<Option<String>, _>("tool_state")?.as_deref()
                        == Some("succeeded")
                        && row
                            .try_get::<Option<String>, _>("revoked_directory")?
                            .is_none() =>
                {
                    "tool_result_available"
                }
                "cancelled" | "failed" | "recovery_required" | "completed" | "paused" => "stopped",
                _ if job.is_some() => "submitted",
                _ if proposal.is_some() => "ready_to_quote",
                _ => "awaiting_workspace",
            }
        };
        let mut result = json!({"run_id":run,"user_id":actor.user,"owner_id":owner,"root_budget_id":row.try_get::<String,_>("root_budget_id")?,
            "input_version_id":row.try_get::<String,_>("input_version_id")?,"model_version":row.try_get::<String,_>("model_version")?,
            "price_version":row.try_get::<String,_>("price_version")?,"invocation_id":row.try_get::<String,_>("invocation_id")?,
            "output_limit":row.try_get::<i32,_>("output_limit")?,"stage":stage});
        if let Some(digest) = row.try_get::<Option<String>, _>("tool_digest")? {
            result["tool"] = json!({"name":crate::cloud_tools::NAME,"execution_digest":digest});
        }
        if let Some(job) = job {
            result["job_id"] = json!(job);
        }
        if let Some(projection) = proposal {
            result["request_projection_digest"] = json!(projection);
        }
        authority::finish(tx, actor).await?;
        Ok(result)
    }
    pub(super) async fn confirm_cloud_model(
        &self,
        actor: &ArtifactActor,
        run: &str,
        key: &str,
        c: Confirmation,
    ) -> Result<orange_protocol::JobView, ArtifactError> {
        if !valid_id(run)
            || !valid_id(&c.quote_id)
            || !orange_protocol::valid_digest(&c.request_projection_digest)
        {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let owner: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1")
                .bind(&actor.user)
                .fetch_one(&mut *tx)
                .await?;
        authority::scope(&mut tx, actor, &owner, true).await?;
        let model = crate::cloud_model::live(&mut tx, run, &owner, &actor.user).await?;
        let quote=sqlx::query("SELECT q.max_amount,q.projection_digest,q.invocation_id,q.root_budget_id FROM cloud_model_quotes l JOIN quotes q USING(quote_id)
            JOIN quote_metering m USING(quote_id) WHERE l.quote_id=$1 AND l.run_id=$2 AND l.allocation_id=$3 AND q.owner_id=$4 AND m.requester_id=$5")
            .bind(&c.quote_id).bind(run).bind(model.try_get::<String,_>("allocation_id")?).bind(&owner).bind(&actor.user)
            .fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        if quote.try_get::<i64, _>("max_amount")? != c.max_amount.0.units()
            || quote.try_get::<String, _>("projection_digest")? != c.request_projection_digest
            || model.try_get::<String, _>("request_projection_digest")?
                != c.request_projection_digest
        {
            return Err(ArtifactError::Conflict);
        }
        let req = SubmitJob {
            root_budget_id: quote.try_get("root_budget_id")?,
            invocation_id: quote.try_get("invocation_id")?,
            purpose: InvocationPurpose::Primary,
            quote_id: c.quote_id,
            request_projection_digest: c.request_projection_digest,
        };
        authority::finish(tx, actor).await?;
        let job = crate::store::Store::from_pool(self.inner.pool.clone())
            .submit_cloud(actor, &owner, key, &req, run)
            .await
            .map_err(|e| match e {
                crate::store::StoreError::Budget | crate::store::StoreError::Balance => {
                    ArtifactError::Budget
                }
                crate::store::StoreError::Forbidden => ArtifactError::Forbidden,
                crate::store::StoreError::NotFound => ArtifactError::NotFound,
                crate::store::StoreError::Database(e) => ArtifactError::Database(e),
                _ => ArtifactError::Conflict,
            })?;
        Ok(job)
    }
    pub(super) async fn cloud_result(
        &self,
        actor: &ArtifactActor,
        run: &str,
    ) -> Result<Value, ArtifactError> {
        let meta = self.cloud_status(actor, run).await?;
        if meta["stage"] != "result_available" {
            return Err(ArtifactError::NotReady);
        }
        let owner = meta["owner_id"].as_str().ok_or(ArtifactError::Integrity)?;
        let job = meta["job_id"].as_str().ok_or(ArtifactError::NotReady)?;
        let artifact = self.job_result(actor, owner, job).await?;
        let (_, bytes, _) = self
            .read(actor, owner, &artifact.upload_id, &artifact.version_id)
            .await?;
        let candidate: orange_protocol::model::ResponseCandidate =
            serde_json::from_slice(&bytes.to_bytes()).map_err(|_| ArtifactError::Integrity)?;
        if !candidate.tool_calls.is_empty() || candidate.finish_reason != "stop" {
            return Err(ArtifactError::Integrity);
        }
        let live = self.cloud_status(actor, run).await?;
        if live != meta {
            return Err(ArtifactError::NotReady);
        }
        Ok(
            json!({"run_id":run,"user_id":actor.user,"owner_id":owner,"job_id":job,"response":candidate}),
        )
    }
}
