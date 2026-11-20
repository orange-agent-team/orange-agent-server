//! Model proposals originate from the exact request staged by the platform Host.
//! A proposal and a quote are not a wallet reservation or permission to run a tool.
use crate::{artifacts::ArtifactError, store::StoreError};
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
}
impl Selection {
    pub(crate) fn valid(&self) -> bool {
        valid_id(&self.price_version)
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
            || !req.tools.is_empty()
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
    Ok(())
}
pub(crate) async fn selection(
    tx: &mut Tx<'_>,
    run: &str,
) -> Result<Option<Selection>, sqlx::Error> {
    let row=sqlx::query("SELECT price_version,model_version,invocation_id,output_limit FROM cloud_model_plans WHERE run_id=$1")
        .bind(run).fetch_optional(&mut **tx).await?;
    row.map(|r| {
        Ok(Selection {
            price_version: r.try_get("price_version")?,
            model_version: r.try_get("model_version")?,
            invocation_id: r.try_get("invocation_id")?,
            output_limit: r.try_get::<i32, _>("output_limit")? as u32,
        })
    })
    .transpose()
}
pub(crate) async fn live(
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
        WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 AND r.status IN ('queued','running') AND w.status='ready'
        AND w.lease_until>clock_timestamp() AND w.expires_at>clock_timestamp() AND h.enabled AND h.live_until>clock_timestamp()
        AND h.credential_expires_at>clock_timestamp() AND h.input_protocol_version=1 AND u.active AND u.personal_owner_id=r.owner_id
        AND o.state='active' AND b.state='active' AND a.state='available' AND a.creator_id=r.user_id AND a.resource_id IS NULL
        AND a.media_type='application/json' FOR SHARE OF r,w,h,u,o,b,a").bind(run).bind(owner).bind(user)
        .fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)
}
pub(crate) async fn quote_request(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
    key: &str,
) -> Result<CreateTextQuote, ArtifactError> {
    let r = live(tx, run, owner, user).await?;
    Ok(CreateTextQuote {
        request_id: orange_protocol::request_digest("cloud.quote-key.v1", &(run, key))
            .map_err(|_| ArtifactError::Invalid)?,
        root_budget_id: r.try_get("root_budget_id")?,
        price_version: r.try_get("price_version")?,
        input_upload_id: r.try_get("upload_id")?,
        input_version_id: r.try_get("input_version_id")?,
        output_limit: r.try_get::<i32, _>("output_limit")? as u32,
    })
}
pub(crate) async fn quote_authority(
    tx: &mut Tx<'_>,
    owner: &str,
    user: &str,
    req: &CreateTextQuote,
    run: Option<&str>,
) -> Result<Option<PgRow>, ArtifactError> {
    let origin: Option<String> = sqlx::query_scalar(
        "SELECT run_id FROM cloud_model_plans WHERE input_version_id=$1 AND owner_id=$2",
    )
    .bind(&req.input_version_id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;
    if origin.as_deref() != run {
        return Err(ArtifactError::Forbidden);
    }
    if let Some(id) = run {
        let row = live(tx, id, owner, user).await?;
        if row.try_get::<String, _>("root_budget_id")? != req.root_budget_id
            || row.try_get::<String, _>("price_version")? != req.price_version
            || row.try_get::<String, _>("upload_id")? != req.input_upload_id
            || i64::from(row.try_get::<i32, _>("output_limit")?) != i64::from(req.output_limit)
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
        "SELECT run_id FROM cloud_model_plans WHERE owner_id=$1 AND invocation_id=$2",
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
        let row = live(tx, id, owner, user.ok_or(StoreError::Forbidden)?)
            .await
            .map_err(crate::store::artifact_error)?;
        if row.try_get::<String, _>("request_projection_digest")? != req.request_projection_digest
            || row.try_get::<String, _>("root_budget_id")? != req.root_budget_id
            || req.purpose != orange_protocol::InvocationPurpose::Primary
        {
            return Err(StoreError::Forbidden);
        }
    }
    Ok(())
}

/// Cloud results expose only a settled committed candidate while its original
/// input and owner are still available. Applies to direct artifact reads too.
pub(crate) async fn output_access(tx: &mut Tx<'_>, artifact: &str) -> Result<(), ArtifactError> {
    let run:Option<String>=sqlx::query_scalar("SELECT m.run_id FROM worker_output_uploads w JOIN cloud_model_jobs m USING(job_id) WHERE w.artifact_id=$1")
        .bind(artifact).fetch_optional(&mut **tx).await?;
    let Some(run) = run else {
        return Ok(());
    };
    let available=sqlx::query("SELECT f.run_id FROM cloud_model_results f JOIN cloud_runs r USING(run_id)
        JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id AND a.owner_id=r.owner_id
        WHERE f.run_id=$1 AND f.artifact_id=$2 AND r.status='completed' AND a.state='available' AND a.creator_id=r.user_id FOR SHARE OF r,a")
        .bind(run).bind(artifact).fetch_optional(&mut **tx).await?;
    if available.is_none() {
        return Err(ArtifactError::NotReady);
    }
    Ok(())
}
pub(crate) async fn reconcile_one(pool: &sqlx::PgPool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT r.run_id,w.workspace_id,m.job_id,j.execution_state,j.settlement_state,r.owner_id,r.user_id,p.model_version
        FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_model_jobs m USING(run_id)
        JOIN cloud_model_plans p ON p.run_id=r.run_id JOIN jobs j ON j.job_id=m.job_id AND j.owner_id=r.owner_id AND j.root_budget_id=r.root_budget_id
        WHERE r.status='running' AND w.status='ready' AND (j.execution_state IN ('failed','cancelled') OR
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
        let proof=sqlx::query("SELECT c.commit_id,c.artifact_id,c.version_id,c.response_digest
            FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_pool_hosts h USING(host_instance_id)
            JOIN cloud_model_proposals p USING(run_id) JOIN cloud_model_jobs m USING(run_id)
            JOIN jobs j ON j.job_id=m.job_id AND j.submitting_user_id=r.user_id AND j.quote_id=m.quote_id
            JOIN quote_metering q ON q.quote_id=j.quote_id AND q.input_artifact_id=r.input_artifact_id AND q.input_version_id=r.input_version_id
            JOIN job_response_commits c ON c.job_id=j.job_id JOIN job_usage_facts u ON u.job_id=j.job_id
            JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id
            JOIN artifacts dst ON dst.artifact_id=c.artifact_id AND dst.version_id=c.version_id
            JOIN owners o ON o.owner_id=r.owner_id JOIN users person ON person.user_id=r.user_id AND person.personal_owner_id=r.owner_id
            WHERE r.run_id=$1 AND m.job_id=$2 AND src.owner_id=r.owner_id AND src.state='available' AND dst.owner_id=r.owner_id AND dst.state='available'
              AND u.response_digest=c.response_digest AND u.request_projection_digest=p.request_projection_digest AND NOT u.has_tools
              AND h.enabled AND h.credential_expires_at>clock_timestamp() AND h.live_until>clock_timestamp()
              AND w.lease_until>clock_timestamp() AND w.expires_at>clock_timestamp() AND o.state='active' AND person.active
            FOR SHARE OF h,src,dst,o,person")
            .bind(&run).bind(&job).fetch_optional(&mut *tx).await?;
        if let Some(proof) = proof {
            sqlx::query("INSERT INTO cloud_model_results(run_id,job_id,commit_id,artifact_id,version_id,response_digest) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(&run).bind(&job).bind(proof.try_get::<String,_>("commit_id")?).bind(proof.try_get::<String,_>("artifact_id")?)
                .bind(proof.try_get::<String,_>("version_id")?).bind(proof.try_get::<String,_>("response_digest")?).execute(&mut *tx).await?;
            state = "completed";
            event = "model_response_committed";
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
        SELECT j.job_id,a.artifact_id,a.version_id,r.owner_id,r.user_id,a.sha256,a.size_bytes,p.request_projection_digest
        FROM cloud_runs r JOIN cloud_model_plans plan USING(run_id,owner_id) JOIN cloud_model_proposals p USING(run_id)
        JOIN cloud_workspace_input_reads i ON i.receipt_id=p.input_receipt_id AND i.allocation_id=p.allocation_id
        JOIN artifacts a ON a.artifact_id=i.artifact_id AND a.version_id=i.version_id AND a.sha256=i.sha256
        JOIN cloud_model_quotes cq ON cq.run_id=r.run_id AND cq.allocation_id=p.allocation_id JOIN quotes q ON q.quote_id=cq.quote_id
        JOIN quote_metering m ON m.quote_id=q.quote_id JOIN quote_execution_plans ep ON ep.quote_id=q.quote_id
        JOIN jobs j ON j.quote_id=q.quote_id AND j.owner_id=r.owner_id AND j.root_budget_id=r.root_budget_id
        WHERE r.run_id=$1 AND j.job_id=$2 AND j.submitting_user_id=r.user_id AND j.input_required AND j.execution_state='queued' AND j.settlement_state='pending'
          AND j.invocation_id=plan.invocation_id AND q.invocation_id=j.invocation_id AND q.owner_id=r.owner_id AND q.root_budget_id=r.root_budget_id
          AND q.max_amount=j.reserved AND q.projection_digest=p.request_projection_digest AND ep.request_projection_digest=p.request_projection_digest AND ep.job_deadline>clock_timestamp()
          AND m.requester_id=r.user_id AND m.input_artifact_id=a.artifact_id AND m.input_version_id=a.version_id
          AND plan.input_version_id=a.version_id AND r.input_artifact_id=a.artifact_id AND r.input_version_id=a.version_id
          AND i.model_projection_digest=p.request_projection_digest AND a.owner_id=r.owner_id AND a.creator_id=r.user_id
          AND a.resource_id IS NULL AND a.state='available' AND a.media_type='application/json' AND a.size_bytes BETWEEN 1 AND 262144 RETURNING owner_id,artifact_id,projection_digest")
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
