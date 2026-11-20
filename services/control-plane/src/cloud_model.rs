//! Model proposals originate from the exact request staged by the platform Host.
//! A proposal and a quote are not a wallet reservation or permission to run a tool.
#[path = "cloud_model_lineage.rs"]
mod lineage;
use crate::{artifacts::ArtifactError, store::StoreError};
pub(crate) use lineage::{continuation_lineage, turn_context};
use orange_protocol::{
    model::{GenerateRequest, Role},
    pricing::CreateTextQuote,
    valid_id,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub price_version: String,
    pub model_version: String,
    pub invocation_id: String,
    pub output_limit: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "tool_choice"
    )]
    pub tool: Option<crate::cloud_tools::Choice>,
    #[serde(default, skip_serializing_if = "disabled")]
    pub continue_after_tool: bool,
}
fn tool_choice<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<crate::cloud_tools::Choice>, D::Error> {
    crate::cloud_tools::Choice::deserialize(d).map(Some)
}
fn disabled(v: &bool) -> bool {
    !*v
}
impl Selection {
    pub(crate) fn valid(&self) -> bool {
        (!self.continue_after_tool
            || self
                .tool
                .as_ref()
                .is_some_and(|t| t.name == crate::cloud_tools::NAME))
            && self.tool.as_ref().is_none_or(|t| {
                if t.name == crate::cloud_tools::NAME {
                    crate::cloud_tools::stamp()
                        .is_ok_and(|(_, digest, _)| digest == t.execution_digest)
                } else {
                    crate::hosted_connectors::digest(&t.name).is_ok_and(|d| d == t.execution_digest)
                }
            })
            && valid_id(&self.price_version)
            && valid_id(&self.model_version)
            && valid_id(&self.invocation_id)
            && (1..=4096).contains(&self.output_limit)
    }
    pub(crate) fn projection(&self, bytes: &[u8]) -> Result<String, ArtifactError> {
        let doc = orange_protocol::canonical::parse_tool_arguments(
            std::str::from_utf8(bytes).map_err(|_| ArtifactError::Invalid)?,
        )
        .map_err(|_| ArtifactError::Invalid)?;
        let req: GenerateRequest =
            serde_json::from_value(doc).map_err(|_| ArtifactError::Invalid)?;
        req.validate().map_err(|_| ArtifactError::Invalid)?;
        if req.invocation_id != self.invocation_id
            || req.model_version != self.model_version
            || req.max_output_tokens != self.output_limit
            || req.messages.len() != 1
            || req.messages[0].role != Role::User
            || req.messages[0].content.trim().is_empty()
            || !req.messages[0].tool_calls.is_empty()
            || req.messages[0].tool_call_id.is_some()
            || !match &self.tool {
                None => req.tools.is_empty(),
                Some(choice) => {
                    req.tools.len() == 1
                        && serde_json::to_value(&req.tools[0]).ok()
                            == (if choice.name == crate::cloud_tools::NAME {
                                crate::cloud_tools::definition()
                            } else {
                                crate::hosted_connectors::definition(&choice.name)
                            })
                            .ok()
                            .and_then(|v| serde_json::to_value(v).ok())
                }
            }
        {
            return Err(ArtifactError::Invalid);
        }
        orange_protocol::request_digest("model-request-v1", &req)
            .map_err(|_| ArtifactError::Invalid)
    }
}
pub(crate) async fn register(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    version: &str,
    s: &Selection,
) -> Result<(), StoreError> {
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE owner_id=$1 AND invocation_id=$2)
        OR EXISTS(SELECT 1 FROM quotes WHERE owner_id=$1 AND invocation_id=$2)
        OR EXISTS(SELECT 1 FROM invocation_withdrawals WHERE owner_id=$1 AND invocation_id=$2)",
    )
    .bind(owner)
    .bind(&s.invocation_id)
    .fetch_one(&mut **tx)
    .await?;
    if used {
        return Err(StoreError::Conflict);
    }
    let reviewed=sqlx::query("SELECT p.price_version FROM text_prices p JOIN reviewed_execution_profiles e USING(execution_profile_id)
        JOIN trusted_meter_profiles m ON p.meter_profile_digest=m.profile_digest WHERE p.price_version=$1 AND p.enabled AND e.enabled
        AND m.enabled AND p.admission='development_only' AND e.model_version_id=$2 AND p.output_limit>=$3 FOR SHARE OF p,e,m")
        .bind(&s.price_version).bind(&s.model_version).bind(i64::from(s.output_limit)).fetch_optional(&mut **tx).await?;
    if reviewed.is_none() {
        return Err(StoreError::NotFound);
    }
    let inserted=sqlx::query("INSERT INTO cloud_model_plans(run_id,owner_id,invocation_id,price_version,model_version,output_limit,input_version_id)
        VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING").bind(run).bind(owner).bind(&s.invocation_id).bind(&s.price_version)
        .bind(&s.model_version).bind(s.output_limit as i32).bind(version).execute(&mut **tx).await?;
    if inserted.rows_affected() != 1 {
        return Err(StoreError::Conflict);
    }
    if let Some(choice) = &s.tool {
        if let Some(family) = crate::hosted_connectors::family(&choice.name) {
            if crate::hosted_connectors::digest(&choice.name).map_err(|_| StoreError::Invalid)?
                != choice.execution_digest
            {
                return Err(StoreError::Conflict);
            }
            sqlx::query("INSERT INTO cloud_connector_plans(run_id,tool_name,connector_kind,execution_digest) VALUES($1,$2,$3,$4)")
                .bind(run).bind(&choice.name).bind(family).bind(&choice.execution_digest).execute(&mut **tx).await?;
        } else {
            let (version, digest, policy) =
                crate::cloud_tools::stamp().map_err(|_| StoreError::Invalid)?;
            if choice.execution_digest != digest {
                return Err(StoreError::Conflict);
            }
            sqlx::query("INSERT INTO cloud_directory_capabilities(run_id,capability_id,workspace_id,tool_version,execution_digest,policy_version,created_by)
            SELECT r.run_id,$2,w.workspace_id,$3,$4,$5,r.user_id FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) WHERE r.run_id=$1")
            .bind(run).bind(uuid::Uuid::new_v4().to_string()).bind(version).bind(digest).bind(policy).execute(&mut **tx).await?;
        }
    }
    if s.continue_after_tool {
        sqlx::query("INSERT INTO cloud_continuation_options(run_id,authorized_by) SELECT run_id,user_id FROM cloud_runs WHERE run_id=$1")
            .bind(run).execute(&mut **tx).await?;
    }
    Ok(())
}
pub(crate) async fn selection(
    tx: &mut Tx<'_>,
    run: &str,
) -> Result<Option<Selection>, sqlx::Error> {
    let row=sqlx::query("SELECT p.price_version,p.model_version,p.invocation_id,p.output_limit,COALESCE(c.execution_digest,cp.execution_digest) AS execution_digest,COALESCE(cp.tool_name,'list_directory') AS tool_name,EXISTS(SELECT 1 FROM cloud_continuation_options o WHERE o.run_id=p.run_id) AS followup FROM cloud_model_plans p LEFT JOIN cloud_directory_capabilities c USING(run_id) LEFT JOIN cloud_connector_plans cp USING(run_id) WHERE p.run_id=$1")
        .bind(run).fetch_optional(&mut **tx).await?;
    row.map(|r| {
        Ok(Selection {
            price_version: r.try_get("price_version")?,
            model_version: r.try_get("model_version")?,
            invocation_id: r.try_get("invocation_id")?,
            continue_after_tool: r.try_get("followup")?,
            tool: r
                .try_get::<Option<String>, _>("execution_digest")?
                .map(|execution_digest| crate::cloud_tools::Choice {
                    name: r.get("tool_name"),
                    execution_digest,
                }),
            output_limit: r.try_get::<i32, _>("output_limit")? as u32,
        })
    })
    .transpose()
}
pub(crate) async fn workspace_context(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
) -> Result<PgRow, ArtifactError> {
    sqlx::query("SELECT p.*,r.user_id,r.root_budget_id,r.input_artifact_id,r.status,w.workspace_id,w.epoch,
        m.allocation_id,m.request_projection_digest,upl.upload_id FROM cloud_runs r JOIN cloud_model_plans p USING(run_id,owner_id)
        JOIN cloud_model_proposals m USING(run_id) JOIN cloud_workspaces w USING(run_id,owner_id)
        JOIN cloud_workspace_claims claim ON claim.allocation_id=m.allocation_id AND claim.workspace_id=w.workspace_id AND claim.epoch=w.epoch
        JOIN cloud_pool_hosts h ON h.host_instance_id=w.host_instance_id JOIN users u ON u.user_id=r.user_id
        JOIN owners o ON o.owner_id=r.owner_id JOIN budget_roots b ON b.root_budget_id=r.root_budget_id AND b.owner_id=r.owner_id
        JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=p.input_version_id AND a.owner_id=r.owner_id
        JOIN artifact_uploads upl ON upl.artifact_id=a.artifact_id AND upl.owner_id=r.owner_id
        WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 AND r.status IN ('queued','running','waiting_approval') AND w.status='ready'
        AND w.lease_until>clock_timestamp() AND w.expires_at>clock_timestamp() AND h.enabled AND h.live_until>clock_timestamp()
        AND h.credential_expires_at>clock_timestamp() AND h.input_protocol_version=1 AND u.active AND u.personal_owner_id=r.owner_id
        AND o.state='active' AND b.state='active' AND a.state='available' AND a.creator_id=r.user_id AND a.resource_id IS NULL
        AND a.media_type='application/json' FOR SHARE OF r,w,h,u,o,b,a").bind(run).bind(owner).bind(user)
        .fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)
}
pub(crate) async fn live(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
) -> Result<PgRow, ArtifactError> {
    let row = workspace_context(tx, run, owner, user).await?;
    if row.try_get::<i32, _>("execution_protocol")? > 0 {
        let valid = sqlx::query(
            "SELECT epoch FROM cloud_run_leases WHERE run_id=$1 AND allocation_id=$2
            AND lease_until>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint FOR SHARE",
        )
        .bind(run)
        .bind(row.try_get::<String, _>("allocation_id")?)
        .fetch_optional(&mut **tx)
        .await?;
        if valid.is_none() {
            return Err(ArtifactError::NotReady);
        }
    }
    Ok(row)
}
pub(crate) async fn quote_request(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
    key: &str,
) -> Result<CreateTextQuote, ArtifactError> {
    let r = turn_context(tx, run, owner, user, None).await?;
    Ok(CreateTextQuote {
        request_id: orange_protocol::request_digest("cloud.quote-key.v1", &(r.turn_id, key))
            .map_err(|_| ArtifactError::Invalid)?,
        root_budget_id: r.root_budget_id,
        price_version: r.price_version,
        input_upload_id: r.upload_id,
        input_version_id: r.input_version_id,
        output_limit: r.output_limit,
    })
}
pub(crate) async fn quote_authority(
    tx: &mut Tx<'_>,
    owner: &str,
    user: &str,
    req: &CreateTextQuote,
    run: Option<&str>,
) -> Result<Option<lineage::TurnContext>, ArtifactError> {
    let origin: Option<String> = sqlx::query_scalar(
        "SELECT run_id FROM cloud_model_plans WHERE input_version_id=$1 AND owner_id=$2
        UNION SELECT run_id FROM cloud_model_turns WHERE version_id=$1 AND owner_id=$2",
    )
    .bind(&req.input_version_id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;
    if origin.as_deref() != run {
        return Err(ArtifactError::Forbidden);
    }
    if let Some(id) = run {
        let row = turn_context(tx, id, owner, user, Some(&req.input_version_id)).await?;
        if row.root_budget_id != req.root_budget_id
            || row.price_version != req.price_version
            || row.upload_id != req.input_upload_id
            || row.output_limit != req.output_limit
        {
            return Err(ArtifactError::Conflict);
        }
        Ok(Some(row))
    } else {
        Ok(None)
    }
}

pub(crate) async fn check_quote_origin(
    tx: &mut Tx<'_>,
    quote: &str,
    origin: Option<&str>,
) -> Result<(), ArtifactError> {
    let actual: Option<String> =
        sqlx::query_scalar("SELECT run_id FROM cloud_model_quotes WHERE quote_id=$1")
            .bind(quote)
            .fetch_optional(&mut **tx)
            .await?;
    if actual.as_deref() != origin {
        return Err(ArtifactError::Conflict);
    }
    Ok(())
}
pub(crate) async fn authorize_submit(
    tx: &mut Tx<'_>,
    owner: &str,
    user: Option<&str>,
    req: &orange_protocol::SubmitJob,
    origin: Option<&str>,
) -> Result<(), StoreError> {
    let linked: Option<String> = sqlx::query_scalar(
        "SELECT run_id FROM cloud_model_plans WHERE owner_id=$1 AND invocation_id=$2
        UNION SELECT run_id FROM cloud_model_turns WHERE owner_id=$1 AND invocation_id=$2
        UNION SELECT run_id FROM cloud_continuation_intents WHERE owner_id=$1 AND invocation_id=$2",
    )
    .bind(owner)
    .bind(&req.invocation_id)
    .fetch_optional(&mut **tx)
    .await?;
    if linked.as_deref() != origin {
        return Err(StoreError::Forbidden);
    }
    check_quote_origin(tx, &req.quote_id, origin)
        .await
        .map_err(crate::store::artifact_error)?;
    if let Some(id) = origin {
        let row = turn_context(tx, id, owner, user.ok_or(StoreError::Forbidden)?, None)
            .await
            .map_err(crate::store::artifact_error)?;
        if row.invocation_id != req.invocation_id
            || row.projection != req.request_projection_digest
            || row.root_budget_id != req.root_budget_id
            || req.purpose
                != if row.ordinal == 1 {
                    orange_protocol::InvocationPurpose::Primary
                } else {
                    orange_protocol::InvocationPurpose::Continuation
                }
        {
            return Err(StoreError::Forbidden);
        }
    }
    Ok(())
}

/// Cloud results expose only a settled committed candidate while its original
/// input and owner are still available. Applies to direct artifact reads too.
pub(crate) async fn output_access(tx: &mut Tx<'_>, artifact: &str) -> Result<(), ArtifactError> {
    let derived=sqlx::query("SELECT i.run_id,i.owner_id,i.user_id,t.turn_id FROM artifact_uploads u
        JOIN cloud_continuation_intents i ON i.owner_id=u.owner_id AND i.upload_id=u.upload_id
        LEFT JOIN cloud_model_turns t ON t.turn_id=i.turn_id AND t.artifact_id=u.artifact_id WHERE u.artifact_id=$1")
        .bind(artifact).fetch_optional(&mut **tx).await?;
    if let Some(row) = derived {
        if row.try_get::<Option<String>, _>("turn_id")?.is_none() {
            return Err(ArtifactError::NotReady);
        }
        continuation_lineage(
            tx,
            row.try_get("run_id")?,
            row.try_get("owner_id")?,
            row.try_get("user_id")?,
        )
        .await?;
    }
    let job:Option<String>=sqlx::query_scalar("SELECT m.job_id FROM worker_output_uploads w JOIN cloud_model_jobs m USING(job_id) WHERE w.artifact_id=$1")
        .bind(artifact).fetch_optional(&mut **tx).await?;
    if let Some(job) = job {
        let row=sqlx::query("SELECT r.run_id,r.owner_id,r.user_id,t.ordinal FROM cloud_model_results f JOIN cloud_runs r USING(run_id)
            JOIN cloud_job_turns jt ON jt.job_id=f.job_id JOIN cloud_model_turns t ON t.turn_id=jt.turn_id
            JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id AND a.owner_id=r.owner_id
            JOIN artifacts model_input ON model_input.artifact_id=t.artifact_id AND model_input.version_id=t.version_id
            WHERE f.job_id=$1 AND f.artifact_id=$2 AND r.status='completed' AND a.state='available' AND a.creator_id=r.user_id
              AND model_input.state='available' AND model_input.owner_id=r.owner_id FOR SHARE OF r,a,model_input")
            .bind(job).bind(artifact).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)?;
        if row.try_get::<i32, _>("ordinal")? == 2 {
            continuation_lineage(
                tx,
                row.try_get("run_id")?,
                row.try_get("owner_id")?,
                row.try_get("user_id")?,
            )
            .await?;
        }
    }
    Ok(())
}
pub(crate) async fn reconcile_one(pool: &sqlx::PgPool) -> Result<bool, sqlx::Error> {
    if crate::hosted_connectors::reconcile(pool).await? {
        return Ok(true);
    }
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT r.run_id,w.workspace_id,m.job_id,jt.turn_id,turn.ordinal,j.execution_state,j.settlement_state,r.owner_id,r.user_id,p.model_version
        FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_model_jobs m USING(run_id)
        JOIN cloud_job_turns jt ON jt.job_id=m.job_id JOIN cloud_model_turns turn ON turn.turn_id=jt.turn_id
        JOIN cloud_model_plans p ON p.run_id=r.run_id JOIN jobs j ON j.job_id=m.job_id AND j.owner_id=r.owner_id AND j.root_budget_id=r.root_budget_id
        WHERE r.status='running' AND w.status='ready' AND NOT EXISTS(SELECT 1 FROM cloud_model_outcomes done WHERE done.turn_id=jt.turn_id) AND (j.execution_state IN ('failed','cancelled') OR
         (j.execution_state='succeeded' AND j.settlement_state='settled')) ORDER BY r.sequence LIMIT 1 FOR UPDATE OF r,w SKIP LOCKED")
        .fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(false);
    };
    let run: String = row.try_get("run_id")?;
    let job: String = row.try_get("job_id")?;
    let mut state = "failed";
    let mut event = "model_job_stopped";
    if row.try_get::<String, _>("execution_state")? == "succeeded" {
        // Device output, meter facts, published version and originating input are
        // all checked before a model response becomes the public Run result.
        let proof=sqlx::query("SELECT c.commit_id,c.artifact_id,c.version_id,c.response_digest,u.has_tools
            FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_pool_hosts h USING(host_instance_id)
            JOIN cloud_model_plans plan USING(run_id) JOIN cloud_model_proposals p USING(run_id) JOIN cloud_model_jobs m USING(run_id)
            JOIN jobs j ON j.job_id=m.job_id AND j.submitting_user_id=r.user_id AND j.quote_id=m.quote_id
            JOIN cloud_job_turns jt ON jt.job_id=j.job_id JOIN cloud_model_turns turn ON turn.turn_id=jt.turn_id AND turn.run_id=r.run_id
            JOIN quote_metering q ON q.quote_id=j.quote_id AND q.input_artifact_id=turn.artifact_id AND q.input_version_id=turn.version_id
            JOIN artifacts model_input ON model_input.artifact_id=turn.artifact_id AND model_input.version_id=turn.version_id
            JOIN job_response_commits c ON c.job_id=j.job_id JOIN job_usage_facts u ON u.job_id=j.job_id
            JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id
            JOIN artifacts dst ON dst.artifact_id=c.artifact_id AND dst.version_id=c.version_id
            JOIN owners o ON o.owner_id=r.owner_id JOIN users person ON person.user_id=r.user_id AND person.personal_owner_id=r.owner_id
            WHERE r.run_id=$1 AND m.job_id=$2 AND src.owner_id=r.owner_id AND src.state='available' AND dst.owner_id=r.owner_id AND dst.state='available'
              AND u.response_digest=c.response_digest AND u.request_projection_digest=turn.projection_digest AND model_input.state='available' AND model_input.owner_id=r.owner_id
              AND (plan.execution_protocol=0 OR EXISTS(SELECT 1 FROM cloud_run_leases lease WHERE lease.run_id=r.run_id AND lease.allocation_id=p.allocation_id
                  AND lease.lease_until>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint))
              AND h.enabled AND h.credential_expires_at>clock_timestamp() AND h.live_until>clock_timestamp()
              AND w.lease_until>clock_timestamp() AND w.expires_at>clock_timestamp() AND o.state='active' AND person.active
            FOR SHARE OF h,src,dst,o,person,model_input")
            .bind(&run).bind(&job).fetch_optional(&mut *tx).await?;
        let lineage_live = if row.try_get::<i32, _>("ordinal")? == 2 {
            match continuation_lineage(
                &mut tx,
                &run,
                row.try_get("owner_id")?,
                row.try_get("user_id")?,
            )
            .await
            {
                Ok(_) => true,
                Err(ArtifactError::NotReady) => false,
                Err(ArtifactError::Database(e)) => return Err(e),
                Err(_) => {
                    return Err(sqlx::Error::Protocol(
                        "unreadable cloud model lineage".into(),
                    ));
                }
            }
        } else {
            true
        };
        if let Some(proof) = proof.filter(|_| lineage_live) {
            if proof.try_get::<bool, _>("has_tools")? {
                let count=sqlx::query("INSERT INTO cloud_tool_responses(run_id,job_id,commit_id,artifact_id,version_id,response_digest)
                    SELECT $1,$2,$3,$4,$5,$6 FROM cloud_directory_capabilities c WHERE run_id=$1 AND $7=1 AND NOT EXISTS
                    (SELECT 1 FROM cloud_directory_revocations v WHERE v.run_id=c.run_id)")
                    .bind(&run).bind(&job).bind(proof.try_get::<String,_>("commit_id")?).bind(proof.try_get::<String,_>("artifact_id")?)
                    .bind(proof.try_get::<String,_>("version_id")?).bind(proof.try_get::<String,_>("response_digest")?).bind(row.try_get::<i32,_>("ordinal")?).execute(&mut *tx).await?.rows_affected();
                let count = if count == 0 && row.try_get::<i32, _>("ordinal")? == 1 {
                    sqlx::query("INSERT INTO cloud_connector_model_commits(run_id,job_id,commit_id,artifact_id,version_id,response_digest)
                        SELECT run_id,$2,$3,$4,$5,$6 FROM cloud_connector_plans WHERE run_id=$1").bind(&run).bind(&job)
                        .bind(proof.try_get::<String,_>("commit_id")?).bind(proof.try_get::<String,_>("artifact_id")?).bind(proof.try_get::<String,_>("version_id")?)
                        .bind(proof.try_get::<String,_>("response_digest")?).execute(&mut *tx).await?.rows_affected()
                } else {
                    count
                };
                if count == 1 {
                    let rev:i64=sqlx::query_scalar("UPDATE cloud_runs SET status='waiting_approval',version=version+1 WHERE run_id=$1 RETURNING version")
                        .bind(&run).fetch_one(&mut *tx).await?;
                    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,'model_tool_response_committed')").bind(run).bind(rev).execute(&mut *tx).await?;
                    record_outcome(&mut tx, row.try_get("turn_id")?, &job, "tool_request").await?;
                    tx.commit().await?;
                    return Ok(true);
                }
                state = "recovery_required";
                event = "unsupported_tool_response";
            } else {
                sqlx::query("INSERT INTO cloud_model_results(run_id,job_id,commit_id,artifact_id,version_id,response_digest) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(&run).bind(&job).bind(proof.try_get::<String,_>("commit_id")?).bind(proof.try_get::<String,_>("artifact_id")?)
                .bind(proof.try_get::<String,_>("version_id")?).bind(proof.try_get::<String,_>("response_digest")?).execute(&mut *tx).await?;
                state = "completed";
                event = "model_response_committed";
            }
        } else {
            state = "recovery_required";
            event = "model_response_unavailable";
        }
    }
    let rev: i64 = sqlx::query_scalar(
        "UPDATE cloud_runs SET status=$2,version=version+1 WHERE run_id=$1 RETURNING version",
    )
    .bind(&run)
    .bind(state)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE cloud_workspaces SET status='draining',lease_until=NULL,epoch=epoch+1,version=version+1 WHERE workspace_id=$1 AND status='ready'")
        .bind(row.try_get::<String,_>("workspace_id")?).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
        .bind(run)
        .bind(rev)
        .bind(event)
        .execute(&mut *tx)
        .await?;
    record_outcome(
        &mut tx,
        row.try_get("turn_id")?,
        &job,
        if state == "completed" {
            "final"
        } else {
            "stopped"
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// The original proposal already read and hashed the immutable model input;
/// trusted quote metering independently validated the same projection. Bind the
/// exact version and reserve funds in the same transaction, before a Worker
/// can observe the new Job.
pub(crate) async fn bind_submitted_input(
    tx: &mut Tx<'_>,
    run: &str,
    job: &str,
) -> Result<(), StoreError> {
    let bound=sqlx::query("INSERT INTO job_inputs(job_id,artifact_id,version_id,owner_id,authorizing_user_id,artifact_sha256,size_bytes,projection_digest)
        SELECT j.job_id,a.artifact_id,a.version_id,r.owner_id,r.user_id,a.sha256,a.size_bytes,t.projection_digest
        FROM jobs j JOIN cloud_quote_turns qt ON qt.quote_id=j.quote_id JOIN cloud_model_turns t ON t.turn_id=qt.turn_id AND t.run_id=qt.run_id
        JOIN cloud_runs r ON r.run_id=t.run_id AND r.owner_id=t.owner_id JOIN artifacts a ON a.artifact_id=t.artifact_id AND a.version_id=t.version_id
        JOIN quotes q ON q.quote_id=j.quote_id JOIN quote_metering m ON m.quote_id=q.quote_id JOIN quote_execution_plans ep ON ep.quote_id=q.quote_id
        WHERE r.run_id=$1 AND j.job_id=$2 AND j.owner_id=r.owner_id AND j.root_budget_id=r.root_budget_id AND j.submitting_user_id=r.user_id
          AND j.input_required AND j.execution_state='queued' AND j.settlement_state='pending' AND j.invocation_id=t.invocation_id AND q.invocation_id=j.invocation_id
          AND q.owner_id=r.owner_id AND q.root_budget_id=r.root_budget_id AND q.max_amount=j.reserved AND q.projection_digest=t.projection_digest
          AND ep.request_projection_digest=t.projection_digest AND ep.job_deadline>clock_timestamp() AND m.requester_id=r.user_id AND m.input_artifact_id=a.artifact_id
          AND m.input_version_id=a.version_id AND a.owner_id=r.owner_id AND a.creator_id=r.user_id AND a.resource_id IS NULL AND a.state='available' AND a.media_type='application/json'
          AND a.size_bytes BETWEEN 1 AND 262144 RETURNING owner_id,artifact_id,projection_digest")
        .bind(run).bind(job).fetch_optional(&mut **tx).await?.ok_or(StoreError::Conflict)?;
    crate::artifacts::inputs::record_input_bound(
        tx,
        bound.try_get("owner_id")?,
        job,
        bound.try_get("artifact_id")?,
        bound.try_get("projection_digest")?,
    )
    .await?;
    Ok(())
}
async fn record_outcome(
    tx: &mut Tx<'_>,
    turn: &str,
    job: &str,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO cloud_model_outcomes(turn_id,job_id,kind) VALUES($1,$2,$3)")
        .bind(turn)
        .bind(job)
        .bind(kind)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
