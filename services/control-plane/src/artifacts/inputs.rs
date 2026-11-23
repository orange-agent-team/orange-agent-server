//! User-authorized immutable transport binding for the existing text model
//! request protocol. Binding is not model execution or a pricing certification.
use super::*;
use orange_protocol::{
    SubmitJob,
    artifacts::{BindJobInput, JobInputView},
    model::GenerateRequest,
};
impl ArtifactStore {
    pub async fn job_by_invocation(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        invocation: &str,
    ) -> Result<orange_protocol::artifacts::UserJobLookup, ArtifactError> {
        if !valid_id(invocation) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let row=sqlx::query("SELECT * FROM jobs WHERE owner_id=$1 AND submitting_user_id=$2 AND invocation_id=$3 FOR SHARE").bind(owner).bind(&actor.user).bind(invocation).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        let result = orange_protocol::artifacts::UserJobLookup {
            owner_id: owner.into(),
            user_id: actor.user.clone(),
            quote_id: row.try_get("quote_id")?,
            job: crate::store::job_view(&row).map_err(|_| ArtifactError::Integrity)?,
        };
        authority::finish(tx, actor).await?;
        Ok(result)
    }

    pub async fn jobs_for_user(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        after: &str,
        limit: u16,
    ) -> Result<orange_protocol::artifacts::UserJobList, ArtifactError> {
        if (!after.is_empty() && !valid_id(after)) || !(1..=100).contains(&limit) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let rows=sqlx::query("SELECT * FROM jobs WHERE owner_id=$1 AND submitting_user_id=$2 AND job_id>$3 ORDER BY job_id LIMIT $4").bind(owner).bind(&actor.user).bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        let mut data = Vec::new();
        for row in rows.iter().take(usize::from(limit)) {
            data.push(crate::store::job_view(row).map_err(|_| ArtifactError::Integrity)?);
        }
        let next_cursor = if more {
            data.last().map(|job| job.job_id.clone())
        } else {
            None
        };
        authority::finish(tx, actor).await?;
        Ok(orange_protocol::artifacts::UserJobList {
            owner_id: owner.into(),
            user_id: actor.user.clone(),
            data,
            next_cursor,
        })
    }

    pub async fn submit_job(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        key: &str,
        request: &SubmitJob,
    ) -> Result<orange_protocol::JobView, crate::store::StoreError> {
        crate::store::Store::from_pool(self.inner.pool.clone())
            .submit_for_user(actor, owner, key, request)
            .await
    }
    pub async fn job_for_user(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        job: &str,
    ) -> Result<orange_protocol::JobView, ArtifactError> {
        use orange_domain::identity::{JobId, OwnerId};
        Ok(self
            .submitted_job(
                actor,
                OwnerId::try_from(owner).map_err(|_| ArtifactError::Invalid)?,
                JobId::try_from(job).map_err(|_| ArtifactError::Invalid)?,
            )
            .await?
            .view)
    }

    pub(super) async fn submitted_job(
        &self,
        actor: &ArtifactActor,
        owner: orange_domain::identity::OwnerId<'_>,
        job: orange_domain::identity::JobId<'_>,
    ) -> Result<crate::job_revision::Status, ArtifactError> {
        use orange_domain::identity::{SubmittedJob, UserId};
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner.as_str(), false).await?;
        let resource = SubmittedJob {
            owner,
            job,
            submitter: UserId::try_from(actor.user.as_str()).map_err(|_| ArtifactError::Invalid)?,
        };
        let row=sqlx::query("SELECT * FROM jobs WHERE job_id=$1 AND owner_id=$2 AND submitting_user_id=$3 FOR SHARE").bind(resource.job.as_str()).bind(resource.owner.as_str()).bind(resource.submitter.as_str()).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        let result = crate::store::job_view(&row).map_err(|_| ArtifactError::Integrity)?;
        let revision = row.try_get("state_revision")?;
        authority::finish(tx, actor).await?;
        Ok(crate::job_revision::Status {
            view: result,
            revision,
        })
    }
    pub async fn bind_job_input(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        job: &str,
        request: &BindJobInput,
    ) -> Result<JobInputView, ArtifactError> {
        if !valid_id(job) || !actor.write {
            return Err(ArtifactError::Invalid);
        }
        let metadata = self.status(actor, owner, &request.upload_id).await?;
        let size = metadata
            .size_bytes
            .parse::<u64>()
            .map_err(|_| ArtifactError::Integrity)?;
        if metadata.media_type != "application/json"
            || size == 0
            || size > 256 * 1024
            || metadata.version_id != request.version_id
        {
            return Err(ArtifactError::Invalid);
        }
        let (metadata, bytes, stamp) = self
            .read(actor, owner, &request.upload_id, &request.version_id)
            .await?;
        let text = std::str::from_utf8(&bytes.to_bytes())
            .map_err(|_| ArtifactError::Invalid)?
            .to_owned();
        let value = orange_protocol::canonical::parse_tool_arguments(&text)
            .map_err(|_| ArtifactError::Invalid)?;
        let input: GenerateRequest =
            serde_json::from_value(value).map_err(|_| ArtifactError::Invalid)?;
        input.validate().map_err(|_| ArtifactError::Invalid)?;
        let projection =
            request_digest("model-request-v1", &input).map_err(|_| ArtifactError::Invalid)?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        let root:Option<String>=sqlx::query_scalar("SELECT root_budget_id FROM jobs WHERE job_id=$1 AND owner_id=$2 AND submitting_user_id=$3 AND input_required").bind(job).bind(owner).bind(&actor.user).fetch_optional(&mut *tx).await?;
        let root = root.ok_or(ArtifactError::Forbidden)?;
        sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE").bind(&root).bind(owner).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        crate::store::check_budget_actor(&mut tx, &actor.user, owner, &root)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let row=sqlx::query("SELECT j.*,q.projection_digest AS quote_projection,p.request_projection_digest AS plan_projection,r.model_version_id FROM jobs j JOIN quotes q USING(quote_id) JOIN quote_execution_plans p USING(quote_id) JOIN reviewed_execution_profiles r USING(execution_profile_id) WHERE j.job_id=$1 AND j.owner_id=$2 AND j.submitting_user_id=$3 AND j.input_required AND r.enabled AND q.owner_id=j.owner_id AND q.root_budget_id=j.root_budget_id AND q.invocation_id=j.invocation_id AND q.max_amount=j.reserved AND p.job_deadline>clock_timestamp() FOR UPDATE OF j FOR SHARE OF r").bind(job).bind(owner).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        if input.invocation_id != row.try_get::<String, _>("invocation_id")?
            || input.model_version != row.try_get::<String, _>("model_version_id")?
            || projection != row.try_get::<String, _>("quote_projection")?
            || projection != row.try_get::<String, _>("plan_projection")?
        {
            return Err(ArtifactError::Conflict);
        }
        let submitted = SubmitJob {
            root_budget_id: root,
            invocation_id: input.invocation_id,
            purpose: serde_json::from_value(serde_json::Value::String(row.try_get("purpose")?))
                .map_err(|_| ArtifactError::Integrity)?,
            quote_id: row.try_get("quote_id")?,
            request_projection_digest: projection.clone(),
        };
        if request_digest("submit_job", &submitted).map_err(|_| ArtifactError::Invalid)?
            != row.try_get::<String, _>("request_digest")?
        {
            return Err(ArtifactError::Integrity);
        }
        let current = self.row_in(&mut tx, owner, &request.upload_id).await?;
        authority::resource(&mut tx, actor, owner, &current, false, false).await?;
        if view(&current)? != metadata || authority::stamp(&mut tx, &current).await? != stamp {
            return Err(ArtifactError::Forbidden);
        }
        let priced=sqlx::query("SELECT requester_id,input_artifact_id,input_version_id FROM quote_metering WHERE quote_id=$1").bind(row.try_get::<String,_>("quote_id")?).fetch_optional(&mut *tx).await?;
        if let Some(priced) = priced
            && (priced.try_get::<String, _>("requester_id")? != actor.user
                || priced.try_get::<String, _>("input_artifact_id")? != metadata.artifact_id
                || priced.try_get::<String, _>("input_version_id")? != metadata.version_id)
        {
            return Err(ArtifactError::Conflict);
        }
        if !matches!(
            row.try_get::<String, _>("execution_state")?.as_str(),
            "queued" | "running"
        ) || row.try_get::<String, _>("settlement_state")? != "pending"
        {
            return Err(ArtifactError::Conflict);
        }
        let existing = sqlx::query("SELECT * FROM job_inputs WHERE job_id=$1")
            .bind(job)
            .fetch_optional(&mut *tx)
            .await?;
        let result = JobInputView {
            job_id: job.into(),
            artifact_id: metadata.artifact_id.clone(),
            version_id: metadata.version_id.clone(),
            sha256: metadata.sha256.clone(),
            size_bytes: metadata.size_bytes.clone(),
            projection_digest: projection.clone(),
        };
        if let Some(old) = existing {
            if old.try_get::<String, _>("artifact_id")? != metadata.artifact_id
                || old.try_get::<String, _>("version_id")? != metadata.version_id
                || old.try_get::<String, _>("projection_digest")? != projection
                || old.try_get::<Option<i64>, _>("organization_policy")? != stamp.organization
                || old.try_get::<Option<i64>, _>("acl_version")? != stamp.acl
            {
                return Err(ArtifactError::Conflict);
            }
            authority::finish(tx, actor).await?;
            return Ok(result);
        }
        let attempted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM worker_execution_attempts WHERE job_id=$1)",
        )
        .bind(job)
        .fetch_one(&mut *tx)
        .await?;
        if row.try_get::<String, _>("execution_state")? != "queued"
            || row.try_get::<String, _>("settlement_state")? != "pending"
            || attempted
        {
            return Err(ArtifactError::Conflict);
        }
        sqlx::query("INSERT INTO job_inputs(job_id,artifact_id,version_id,owner_id,authorizing_user_id,artifact_sha256,size_bytes,projection_digest,organization_policy,acl_version) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(job).bind(&metadata.artifact_id).bind(&metadata.version_id).bind(owner).bind(&actor.user).bind(&metadata.sha256).bind(size as i64).bind(&projection).bind(stamp.organization).bind(stamp.acl).execute(&mut *tx).await?;
        record_input_bound(&mut tx, owner, job, &metadata.artifact_id, &projection).await?;
        authority::finish(tx, actor).await?;
        Ok(result)
    }
}
/// Both interactive and cloud-funded submissions must protect the source
/// before a Worker can consume the binding, and publish the same event.
pub(crate) async fn record_input_bound(
    tx: &mut Tx<'_>,
    owner: &str,
    job: &str,
    artifact: &str,
    projection: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,$2)")
        .bind(artifact)
        .bind(format!("job-input:{job}"))
        .execute(&mut **tx)
        .await?;
    crate::events::append(tx, owner, job, "job.input_bound", projection).await?;
    Ok(())
}
/// Original user's current authority is checked for every dispatch/read. A
/// bounded Job grant survives browser logout, but not account/role/ACL changes.
pub(crate) async fn check_input_in(
    tx: &mut Tx<'_>,
    job: &str,
) -> Result<Option<orange_protocol::grpc::worker::InputArtifact>, ArtifactError> {
    let j = sqlx::query("SELECT * FROM jobs WHERE job_id=$1")
        .bind(job)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ArtifactError::NotFound)?;
    if !j.try_get::<bool, _>("input_required")? {
        return Ok(None);
    }
    let owner: String = j.try_get("owner_id")?;
    let user: String = j
        .try_get::<Option<String>, _>("submitting_user_id")?
        .ok_or(ArtifactError::Forbidden)?;
    let role = authority::user_scope(tx, &user, &owner, true).await?;
    crate::store::check_budget_actor(
        tx,
        &user,
        &owner,
        &j.try_get::<String, _>("root_budget_id")?,
    )
    .await
    .map_err(|_| ArtifactError::Forbidden)?;
    let row=sqlx::query("SELECT a.*,i.artifact_sha256 AS bound_sha,i.version_id AS bound_version,i.size_bytes AS bound_size,i.organization_policy,i.acl_version AS bound_acl,i.authorizing_user_id,i.projection_digest FROM job_inputs i JOIN artifacts a USING(artifact_id) WHERE i.job_id=$1 AND i.owner_id=$2 AND a.owner_id=$2 AND a.state='available' FOR SHARE OF a").bind(job).bind(&owner).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)?;
    if row.try_get::<String, _>("authorizing_user_id")? != user
        || row.try_get::<String, _>("sha256")? != row.try_get::<String, _>("bound_sha")?
        || row.try_get::<String, _>("version_id")? != row.try_get::<String, _>("bound_version")?
        || row.try_get::<i64, _>("size_bytes")? != row.try_get::<i64, _>("bound_size")?
    {
        return Err(ArtifactError::Integrity);
    }
    match (role, row.try_get::<Option<String>, _>("resource_id")?) {
        (None, None) => {}
        (Some(role), Some(resource)) => {
            sqlx::query("SELECT resource_id FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND NOT deleted FOR SHARE").bind(&resource).bind(&owner).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
            crate::team::resource_permission(
                tx,
                &user,
                &resource,
                role,
                orange_protocol::team::ResourceAccess::Read,
            )
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        }
        _ => return Err(ArtifactError::Forbidden),
    }
    let projection: String = sqlx::query_scalar(
        "SELECT q.projection_digest FROM jobs j JOIN quotes q USING(quote_id) WHERE j.job_id=$1",
    )
    .bind(job)
    .fetch_one(&mut **tx)
    .await?;
    if row.try_get::<String, _>("projection_digest")? != projection {
        return Err(ArtifactError::Integrity);
    }
    let stamp = authority::stamp(tx, &row).await?;
    if stamp.organization != row.try_get::<Option<i64>, _>("organization_policy")?
        || stamp.acl != row.try_get::<Option<i64>, _>("bound_acl")?
    {
        return Err(ArtifactError::Forbidden);
    }
    let pinned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM artifact_pins WHERE artifact_id=$1 AND reference_id=$2)",
    )
    .bind(row.try_get::<String, _>("artifact_id")?)
    .bind(format!("job-input:{job}"))
    .fetch_one(&mut **tx)
    .await?;
    if !pinned {
        return Err(ArtifactError::Forbidden);
    }
    Ok(Some(orange_protocol::grpc::worker::InputArtifact {
        artifact_id: row.try_get("artifact_id")?,
        version_id: row.try_get("version_id")?,
        sha256: row.try_get("sha256")?,
        size_bytes: row.try_get::<i64, _>("size_bytes")? as u64,
        media_type: row.try_get("media_type")?,
    }))
}

/// Reading a committed derivative needs current content rights, not a new
/// spending/submission grant. Never reuse the stronger live-execution check to
/// deny viewers legitimate historical reads or to restore a revoked reader.
pub(crate) async fn source_read_in(
    tx: &mut Tx<'_>,
    job: &str,
    user: &str,
) -> Result<orange_protocol::grpc::worker::InputArtifact, ArtifactError> {
    let row=sqlx::query("SELECT a.*,i.owner_id AS bound_owner FROM result_source_bindings i JOIN artifacts a USING(artifact_id) WHERE i.job_id=$1 AND a.owner_id=i.owner_id AND a.version_id=i.version_id AND a.sha256=i.artifact_sha256 AND a.size_bytes=i.size_bytes AND a.state='available' FOR SHARE OF a").bind(job).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
    let owner: String = row.try_get("bound_owner")?;
    let role = authority::user_scope(tx, user, &owner, false).await?;
    match (role, row.try_get::<Option<String>, _>("resource_id")?) {
        (None, None) => {}
        (Some(role), Some(resource)) => {
            sqlx::query("SELECT resource_id FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND NOT deleted FOR SHARE").bind(&resource).bind(&owner).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
            crate::team::resource_permission(
                tx,
                user,
                &resource,
                role,
                orange_protocol::team::ResourceAccess::Read,
            )
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        }
        _ => return Err(ArtifactError::Forbidden),
    }
    Ok(orange_protocol::grpc::worker::InputArtifact {
        artifact_id: row.try_get("artifact_id")?,
        version_id: row.try_get("version_id")?,
        sha256: row.try_get("sha256")?,
        size_bytes: row.try_get::<i64, _>("size_bytes")? as u64,
        media_type: row.try_get("media_type")?,
    })
}
