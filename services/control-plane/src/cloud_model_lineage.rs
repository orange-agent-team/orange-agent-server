//! Derivation chain for a bounded second model turn. Revocation or deletion of
//! any parent blocks future quotes, Job starts and both raw and Run result reads.
use super::*;
pub(crate) async fn continuation_lineage(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
) -> Result<PgRow, ArtifactError> {
    sqlx::query("SELECT r.input_artifact_id,r.input_version_id,r.root_budget_id,r.status,cap.capability_id,cap.execution_digest,cap.policy_version,cap.workspace_id,p.request_projection_digest AS first_projection,
        i.operation_id,i.call_id,i.args,i.argument_digest,i.commit_id,o.result,o.result_digest,t.artifact_id AS response_artifact,t.version_id AS response_version,
        t.response_digest,source.sha256 AS original_sha,source.size_bytes AS original_size,st.published_key AS original_key,source.root_id AS original_root,
        response.sha256 AS response_sha,response.size_bytes AS response_size,rt.published_key AS response_key,response.root_id AS response_root,
        plan.invocation_id,plan.model_version,plan.output_limit
        FROM cloud_runs r JOIN cloud_continuation_options opt USING(run_id) JOIN cloud_model_plans plan USING(run_id,owner_id) JOIN cloud_model_proposals p USING(run_id)
        JOIN cloud_directory_capabilities cap USING(run_id) JOIN cloud_operation_intents i ON i.run_id=r.run_id AND i.capability_id=cap.capability_id
        JOIN cloud_operation_state s USING(operation_id) JOIN cloud_operation_results o USING(operation_id) JOIN cloud_operation_decisions d USING(operation_id)
        JOIN cloud_tool_responses t ON t.run_id=r.run_id AND t.commit_id=i.commit_id JOIN job_response_commits c ON c.commit_id=t.commit_id AND c.job_id=t.job_id
        JOIN artifacts source ON source.artifact_id=r.input_artifact_id AND source.version_id=r.input_version_id AND source.owner_id=r.owner_id
        JOIN artifact_transfers st ON st.transfer_id=source.published_transfer_id AND st.artifact_id=source.artifact_id
        JOIN artifacts response ON response.artifact_id=t.artifact_id AND response.version_id=t.version_id AND response.owner_id=r.owner_id
        JOIN artifact_transfers rt ON rt.transfer_id=response.published_transfer_id AND rt.artifact_id=response.artifact_id
        WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 AND opt.authorized_by=r.user_id AND d.user_id=r.user_id
        AND d.decision='approve' AND d.approval_digest=i.approval_digest AND s.state='succeeded' AND o.state='succeeded' AND o.result IS NOT NULL
        AND c.response_digest=t.response_digest AND NOT EXISTS(SELECT 1 FROM cloud_directory_revocations v WHERE v.run_id=r.run_id)
        AND source.state='available' AND response.state='available' AND st.state='attached' AND rt.state='attached' AND NOT st.published_deleted AND NOT rt.published_deleted
        FOR SHARE OF r,source,response,st,rt,s")
        .bind(run).bind(owner).bind(user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)
}
pub(crate) struct TurnContext {
    pub run_id: String,
    pub turn_id: String,
    pub ordinal: i32,
    pub invocation_id: String,
    pub allocation_id: String,
    pub root_budget_id: String,
    pub price_version: String,
    pub upload_id: String,
    pub input_version_id: String,
    pub projection: String,
    pub output_limit: u32,
}
pub(crate) async fn turn_context(
    tx: &mut Tx<'_>,
    run: &str,
    owner: &str,
    user: &str,
    version: Option<&str>,
) -> Result<TurnContext, ArtifactError> {
    let base = live(tx, run, owner, user).await?;
    let row=sqlx::query("SELECT t.run_id,t.turn_id,t.ordinal,t.invocation_id,t.version_id AS input_version_id,t.artifact_id AS input_artifact_id,t.upload_id,
        t.projection_digest AS request_projection_digest,p.price_version,p.model_version,p.output_limit,r.root_budget_id,m.allocation_id,
        a.sha256,a.size_bytes,t.parent_operation FROM cloud_model_turns t JOIN cloud_runs r USING(run_id,owner_id)
        JOIN cloud_model_plans p USING(run_id,owner_id) JOIN cloud_model_proposals m USING(run_id)
        JOIN artifacts a ON a.artifact_id=t.artifact_id AND a.version_id=t.version_id AND a.owner_id=t.owner_id
        WHERE t.run_id=$1 AND t.owner_id=$2 AND r.user_id=$3 AND a.state='available' AND a.creator_id=r.user_id
          AND a.resource_id IS NULL AND a.media_type='application/json' AND a.size_bytes BETWEEN 1 AND 262144
          AND t.ordinal=(SELECT max(ordinal) FROM cloud_model_turns WHERE run_id=$1)
          AND ($4::text IS NULL OR t.version_id=$4) FOR SHARE OF a")
        .bind(run).bind(owner).bind(user).bind(version).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)?;
    if row.try_get::<i32, _>("ordinal")? == 2 {
        let parent = continuation_lineage(tx, run, owner, user).await?;
        if parent.try_get::<String, _>("operation_id")?
            != row
                .try_get::<Option<String>, _>("parent_operation")?
                .ok_or(ArtifactError::Integrity)?
            || parent.try_get::<String, _>("workspace_id")?
                != base.try_get::<String, _>("workspace_id")?
        {
            return Err(ArtifactError::Forbidden);
        }
    }
    Ok(TurnContext {
        run_id: row.try_get("run_id")?,
        turn_id: row.try_get("turn_id")?,
        ordinal: row.try_get("ordinal")?,
        invocation_id: row.try_get("invocation_id")?,
        allocation_id: row.try_get("allocation_id")?,
        root_budget_id: row.try_get("root_budget_id")?,
        price_version: row.try_get("price_version")?,
        upload_id: row.try_get("upload_id")?,
        input_version_id: row.try_get("input_version_id")?,
        projection: row.try_get("request_projection_digest")?,
        output_limit: u32::try_from(row.try_get::<i32, _>("output_limit")?)
            .map_err(|_| ArtifactError::Integrity)?,
    })
}
