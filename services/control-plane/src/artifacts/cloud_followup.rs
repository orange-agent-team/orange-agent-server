use super::*;
use orange_protocol::model::{GenerateRequest, Message, ResponseCandidate, Role};
use serde_json::Value;

#[derive(PartialEq, Eq)]
struct Material {
    owner: String,
    turn: String,
    invocation: String,
    upload: String,
    operation: String,
    original_version: String,
    response_version: String,
    original_root: String,
    original_key: String,
    original_size: u64,
    original_sha: String,
    response_root: String,
    response_key: String,
    response_size: u64,
    response_sha: String,
    model: String,
    limit: u32,
    first_invocation: String,
    call: String,
    arguments: Value,
    first_projection: String,
    operation_digest: String,
    result: Value,
}
fn material(
    row: &sqlx::postgres::PgRow,
    intent: &sqlx::postgres::PgRow,
    owner: &str,
) -> Result<Material, ArtifactError> {
    if row.try_get::<String, _>("operation_id")? != intent.try_get::<String, _>("operation_id")?
        || row.try_get::<String, _>("input_version_id")?
            != intent.try_get::<String, _>("original_version")?
        || row.try_get::<String, _>("response_version")?
            != intent.try_get::<String, _>("model_response_version")?
        || row.try_get::<String, _>("result_digest")?
            != intent.try_get::<String, _>("operation_digest")?
    {
        return Err(ArtifactError::Forbidden);
    }
    let (_, pin, policy) = crate::cloud_tools::stamp().map_err(|_| ArtifactError::Invalid)?;
    if row.try_get::<String, _>("execution_digest")? != pin
        || row.try_get::<String, _>("policy_version")? != policy
    {
        return Err(ArtifactError::Forbidden);
    }
    Ok(Material {
        owner: owner.into(),
        turn: intent.try_get("turn_id")?,
        invocation: intent.try_get("invocation_id")?,
        upload: intent.try_get("upload_id")?,
        operation: row.try_get("operation_id")?,
        original_version: row.try_get("input_version_id")?,
        response_version: row.try_get("response_version")?,
        original_root: row.try_get("original_root")?,
        original_key: row.try_get("original_key")?,
        original_size: u64::try_from(row.try_get::<i64, _>("original_size")?)
            .map_err(|_| ArtifactError::Integrity)?,
        original_sha: row.try_get("original_sha")?,
        response_root: row.try_get("response_root")?,
        response_key: row.try_get("response_key")?,
        response_size: u64::try_from(row.try_get::<i64, _>("response_size")?)
            .map_err(|_| ArtifactError::Integrity)?,
        response_sha: row.try_get("response_sha")?,
        model: row.try_get("model_version")?,
        limit: u32::try_from(row.try_get::<i32, _>("output_limit")?)
            .map_err(|_| ArtifactError::Integrity)?,
        first_invocation: row.try_get("invocation_id")?,
        call: row.try_get("call_id")?,
        arguments: row.try_get("args")?,
        first_projection: row.try_get("first_projection")?,
        operation_digest: row.try_get("result_digest")?,
        result: row.try_get("result")?,
    })
}
impl ArtifactStore {
    async fn followup_sources(
        &self,
        tx: &mut Tx<'_>,
        actor: &ArtifactActor,
        run: &str,
        key: &str,
    ) -> Result<Material, ArtifactError> {
        let owner: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1 AND active")
                .bind(&actor.user)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(ArtifactError::Forbidden)?;
        authority::scope(tx, actor, &owner, true).await?;
        // Serialize retries from different sessions of the same owner, before
        // taking shared source locks. Object reads and writes run outside this transaction.
        sqlx::query("SELECT run_id FROM cloud_runs WHERE run_id=$1 AND owner_id=$2 AND user_id=$3 FOR UPDATE")
            .bind(run).bind(&owner).bind(&actor.user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotFound)?;
        crate::cloud_model::live(tx, run, &owner, &actor.user).await?;
        let row = crate::cloud_model::continuation_lineage(tx, run, &owner, &actor.user).await?;
        if row.try_get::<String, _>("status")? != "waiting_approval" {
            return Err(ArtifactError::NotReady);
        }
        sqlx::query("INSERT INTO cloud_continuation_intents(run_id,turn_id,operation_id,owner_id,user_id,original_version,model_response_version,operation_digest,invocation_id,upload_id,request_key)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT DO NOTHING")
            .bind(run).bind(id()).bind(row.try_get::<String,_>("operation_id")?).bind(&owner).bind(&actor.user)
            .bind(row.try_get::<String,_>("input_version_id")?).bind(row.try_get::<String,_>("response_version")?).bind(row.try_get::<String,_>("result_digest")?)
            .bind(id()).bind(id()).bind(key).execute(&mut **tx).await?;
        let plan=sqlx::query("SELECT * FROM cloud_continuation_intents WHERE run_id=$1 AND owner_id=$2 AND user_id=$3")
            .bind(run).bind(&owner).bind(&actor.user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Conflict)?;
        material(&row, &plan, &owner)
    }
    pub(super) async fn prepare_continuation(
        &self,
        actor: &ArtifactActor,
        run: &str,
        key: &str,
    ) -> Result<Value, ArtifactError> {
        if !valid_id(run) || !valid_id(key) || !actor.write {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let source = self.followup_sources(&mut tx, actor, run, key).await?;
        let ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_model_turns WHERE run_id=$1 AND turn_id=$2)",
        )
        .bind(run)
        .bind(&source.turn)
        .fetch_one(&mut *tx)
        .await?;
        authority::finish(tx, actor).await?;
        if ready {
            return self.cloud_status(actor, run).await;
        }
        let reader = self.reader();
        let first = reader
            .cloud_text_input(
                &source.original_root,
                &source.original_key,
                source.original_size,
                &source.original_sha,
            )
            .await?;
        let reply = reader
            .cloud_text_input(
                &source.response_root,
                &source.response_key,
                source.response_size,
                &source.response_sha,
            )
            .await?;
        let parse = |bytes: &[u8]| {
            orange_protocol::canonical::parse_tool_arguments(
                std::str::from_utf8(bytes).map_err(|_| ArtifactError::Integrity)?,
            )
            .map_err(|_| ArtifactError::Integrity)
        };
        let mut req: GenerateRequest =
            serde_json::from_value(parse(&first)?).map_err(|_| ArtifactError::Integrity)?;
        let model: ResponseCandidate =
            serde_json::from_value(parse(&reply)?).map_err(|_| ArtifactError::Integrity)?;
        if req.model_version != source.model
            || req.invocation_id != source.first_invocation
            || req.max_output_tokens != source.limit
            || req.messages.len() != 1
            || req.messages[0].role != Role::User
            || model.finish_reason != "tool_calls"
        {
            return Err(ArtifactError::Integrity);
        }
        let [call] = model.tool_calls.as_slice() else {
            return Err(ArtifactError::Integrity);
        };
        if call.id != source.call
            || call.name != crate::cloud_tools::NAME
            || call.arguments != source.arguments
        {
            return Err(ArtifactError::Integrity);
        }
        if request_digest("model-request-v1", &req).map_err(|_| ArtifactError::Integrity)?
            != source.first_projection
            || source.result["kind"] != crate::cloud_tools::NAME
            || request_digest("cloud.directory-result.v1", &source.result)
                .map_err(|_| ArtifactError::Integrity)?
                != source.operation_digest
        {
            return Err(ArtifactError::Integrity);
        }
        crate::cloud_tools::model_output(&source.result).map_err(|_| ArtifactError::Integrity)?;
        req.invocation_id = source.invocation.clone();
        req.tools.clear();
        req.messages.insert(0,Message::plain(Role::System,"Answer the user using the committed workspace tool result. No tools are available for this final answer.".into()));
        req.messages.push(Message {
            role: Role::Assistant,
            content: model.text,
            tool_calls: model.tool_calls,
            tool_call_id: None,
        });
        req.messages.push(Message {
            role: Role::Tool,
            content: serde_json::to_string(&source.result).map_err(|_| ArtifactError::Integrity)?,
            tool_calls: vec![],
            tool_call_id: Some(source.call.clone()),
        });
        req.validate().map_err(|_| ArtifactError::Invalid)?;
        let bytes = serde_json::to_vec(&req).map_err(|_| ArtifactError::Invalid)?;
        let projection =
            request_digest("model-request-v1", &req).map_err(|_| ArtifactError::Invalid)?;
        let upload = CreateUpload {
            upload_id: source.upload.clone(),
            size_bytes: bytes.len().to_string(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            media_type: "application/json".into(),
        };
        let mut tx = self.inner.pool.begin().await?;
        if self.followup_sources(&mut tx, actor, run, key).await? != source {
            return Err(ArtifactError::Forbidden);
        }
        authority::finish(tx, actor).await?;
        let initial = self.begin_upload(actor, &source.owner, &upload).await?;
        let artifact = match initial.state.as_str() {
            "staging" => {
                self.put(
                    actor.clone(),
                    source.owner.clone(),
                    source.upload.clone(),
                    tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
                )
                .await?
            }
            "available" => initial,
            _ => return Err(ArtifactError::Deleted),
        };
        if artifact.sha256 != upload.sha256
            || artifact.size_bytes != upload.size_bytes
            || artifact.media_type != upload.media_type
        {
            return Err(ArtifactError::Integrity);
        }
        let mut tx = self.inner.pool.begin().await?;
        if self.followup_sources(&mut tx, actor, run, key).await? != source {
            return Err(ArtifactError::Forbidden);
        }
        let inserted=sqlx::query("INSERT INTO cloud_model_turns(turn_id,run_id,ordinal,owner_id,invocation_id,upload_id,artifact_id,version_id,projection_digest,parent_operation)
            SELECT $1,$2,2,$3,$4,$5,a.artifact_id,a.version_id,$6,$7 FROM artifacts a WHERE a.artifact_id=$8 AND a.version_id=$9 AND a.owner_id=$3
              AND a.creator_id=$10 AND a.state='available' AND a.sha256=$11 AND a.resource_id IS NULL AND a.media_type='application/json'
            ON CONFLICT(turn_id) DO NOTHING").bind(&source.turn).bind(run).bind(&source.owner).bind(&source.invocation).bind(&source.upload)
            .bind(projection).bind(&source.operation).bind(&artifact.artifact_id).bind(&artifact.version_id).bind(&actor.user).bind(&artifact.sha256)
            .execute(&mut *tx).await?;
        crate::cloud_model::turn_context(
            &mut tx,
            run,
            &source.owner,
            &actor.user,
            Some(&artifact.version_id),
        )
        .await?;
        if inserted.rows_affected() == 1 {
            let revision: i64 = sqlx::query_scalar(
                "UPDATE cloud_runs SET version=version+1 WHERE run_id=$1 RETURNING version",
            )
            .bind(run)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,'tool_result_projected_into_model_turn')")
                .bind(run).bind(revision).execute(&mut *tx).await?;
        }
        authority::finish(tx, actor).await?;
        self.cloud_status(actor, run).await
    }
}

impl ArtifactStore {
    pub(super) async fn cloud_history(
        &self,
        actor: &ArtifactActor,
        run: &str,
    ) -> Result<Value, ArtifactError> {
        if !valid_id(run) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let owner: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1")
                .bind(&actor.user)
                .fetch_one(&mut *tx)
                .await?;
        authority::scope(&mut tx, actor, &owner, false).await?;
        let budget:String=sqlx::query_scalar("SELECT root_budget_id FROM cloud_runs r JOIN cloud_model_plans p USING(run_id,owner_id)
            WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3")
            .bind(run).bind(&owner).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        let rows=sqlx::query("SELECT t.turn_id,t.ordinal,t.invocation_id,t.version_id,t.parent_operation,j.job_id,j.execution_state,j.settlement_state,
            j.charge,q.max_amount,q.quote_id,o.kind FROM cloud_model_turns t LEFT JOIN cloud_job_turns jt USING(turn_id,run_id)
            LEFT JOIN jobs j ON j.job_id=jt.job_id AND j.owner_id=t.owner_id LEFT JOIN quotes q ON q.quote_id=j.quote_id
            LEFT JOIN cloud_model_outcomes o ON o.turn_id=t.turn_id WHERE t.run_id=$1 AND t.owner_id=$2 ORDER BY t.ordinal")
            .bind(run).bind(&owner).fetch_all(&mut *tx).await?;
        let mut settled: i128 = 0;
        let mut pending: i128 = 0;
        let mut turns = Vec::new();
        for row in rows {
            let mut v = serde_json::json!({"turn_id":row.try_get::<String,_>("turn_id")?,"ordinal":row.try_get::<i32,_>("ordinal")?,
                "invocation_id":row.try_get::<String,_>("invocation_id")?,"input_version_id":row.try_get::<String,_>("version_id")?});
            if let Some(op) = row.try_get::<Option<String>, _>("parent_operation")? {
                v["parent_operation_id"] = serde_json::json!(op);
            }
            if let Some(kind) = row.try_get::<Option<String>, _>("kind")? {
                v["outcome"] = serde_json::json!(kind);
            }
            if let Some(job) = row.try_get::<Option<String>, _>("job_id")? {
                let state: String = row.try_get("settlement_state")?;
                let charge: i64 = row.try_get("charge")?;
                let maximum: i64 = row.try_get("max_amount")?;
                if charge < 0 || maximum < charge {
                    return Err(ArtifactError::Integrity);
                }
                if state == "settled" {
                    settled += i128::from(charge);
                } else if state == "pending" {
                    pending += i128::from(maximum);
                }
                v["job"] = serde_json::json!({"job_id":job,"quote_id":row.try_get::<String,_>("quote_id")?,"max_microcredits":maximum.to_string(),
                    "charge_microcredits":charge.to_string(),"execution_state":row.try_get::<String,_>("execution_state")?,"settlement_state":state});
            }
            turns.push(v);
        }
        authority::finish(tx, actor).await?;
        Ok(
            serde_json::json!({"run_id":run,"owner_id":owner,"user_id":actor.user,"root_budget_id":budget,"turns":turns,
            "settled_microcredits":settled.to_string(),"pending_max_microcredits":pending.to_string()}),
        )
    }
}
