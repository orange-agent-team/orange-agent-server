//! Read a committed model response before deriving an intent. No candidate
//! text, Host-supplied argument or public Worker identity can mint a permit.
use super::*;
use host::operation_record::State as OpState;
use serde_json::{Value, json};

fn digest(domain: &str, value: &impl serde::Serialize) -> Result<String, Status> {
    orange_protocol::request_digest(domain, value).map_err(|_| changed())
}
#[derive(PartialEq, Eq)]
struct Source {
    commit: String,
    artifact: String,
    version: String,
    root: String,
    key: String,
    size: u64,
    sha: String,
    invocation: String,
    environment: String,
    capability: String,
    tool_version: String,
    policy: String,
}
async fn source(tx: &mut Tx<'_>, run: &str) -> Result<Option<Source>, Status> {
    let row=sqlx::query("SELECT t.commit_id,t.artifact_id,t.version_id,a.root_id,a.size_bytes,a.sha256,x.published_key,p.invocation_id,
        cap.workspace_id,cap.capability_id,cap.execution_digest,cap.tool_version,cap.policy_version FROM cloud_tool_responses t
        JOIN cloud_model_plans p USING(run_id) JOIN cloud_directory_capabilities cap USING(run_id)
        JOIN artifacts a ON a.artifact_id=t.artifact_id AND a.version_id=t.version_id AND a.owner_id=p.owner_id
        JOIN artifact_transfers x ON x.transfer_id=a.published_transfer_id AND x.artifact_id=a.artifact_id
        WHERE t.run_id=$1 AND NOT EXISTS(SELECT 1 FROM cloud_directory_revocations v WHERE v.run_id=t.run_id) AND a.state='available' AND orange_artifact_content_live(a.artifact_id) AND x.state='attached' AND NOT x.published_deleted FOR SHARE OF a,x")
        .bind(run).fetch_optional(&mut **tx).await.map_err(db_error)?;
    let Some(row) = row else {
        let expected: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_tool_responses WHERE run_id=$1)")
                .bind(run)
                .fetch_one(&mut **tx)
                .await
                .map_err(db_error)?;
        if expected {
            return Err(changed());
        }
        return Ok(None);
    };
    let (version, execution, policy) = crate::cloud_tools::stamp().map_err(|_| changed())?;
    if row
        .try_get::<String, _>("execution_digest")
        .map_err(db_error)?
        != execution
        || row.try_get::<String, _>("tool_version").map_err(db_error)? != version
        || row
            .try_get::<String, _>("policy_version")
            .map_err(db_error)?
            != policy
    {
        return Err(changed());
    }
    Ok(Some(Source {
        commit: row.try_get("commit_id").map_err(db_error)?,
        artifact: row.try_get("artifact_id").map_err(db_error)?,
        version: row.try_get("version_id").map_err(db_error)?,
        root: row.try_get("root_id").map_err(db_error)?,
        key: row.try_get("published_key").map_err(db_error)?,
        size: count(row.try_get("size_bytes").map_err(db_error)?)?,
        sha: row.try_get("sha256").map_err(db_error)?,
        invocation: row.try_get("invocation_id").map_err(db_error)?,
        environment: row.try_get("workspace_id").map_err(db_error)?,
        capability: row.try_get("capability_id").map_err(db_error)?,
        tool_version: version,
        policy,
    }))
}
fn parse_call(
    a: &host::RunAuthority,
    s: &Source,
    bytes: &[u8],
) -> Result<(host::IntentRequest, String, Value), Status> {
    let doc = orange_protocol::canonical::parse_tool_arguments(
        std::str::from_utf8(bytes).map_err(|_| changed())?,
    )
    .map_err(|_| changed())?;
    let response: orange_protocol::model::ResponseCandidate =
        serde_json::from_value(doc).map_err(|_| changed())?;
    let [call] = response.tool_calls.as_slice() else {
        return Err(changed());
    };
    if response.finish_reason != "tool_calls"
        || !valid_id(&call.id)
        || call.name != crate::cloud_tools::NAME
    {
        return Err(changed());
    }
    crate::cloud_tools::arguments(&call.arguments).map_err(|_| changed())?;
    let argument_digest = digest("cloud.tool-arguments.v1", &call.arguments)?;
    let operation_id = digest(
        "cloud.tool-operation.v1",
        &(&a.run_id, &s.commit, &s.invocation, &call.id),
    )?;
    let approval_id = digest(
        "cloud.tool-approval.v1",
        &(&operation_id, &argument_digest, &s.capability, &s.policy),
    )?;
    Ok((
        host::IntentRequest {
            authority: Some(a.clone()),
            operation_id,
            invocation_id: s.invocation.clone(),
            response_commit_id: s.commit.clone(),
            tool_name: call.name.clone(),
            tool_version: s.tool_version.clone(),
            argument_digest,
            capability_snapshot_id: s.capability.clone(),
            execution_environment_id: s.environment.clone(),
            policy_version: s.policy.clone(),
            approval_id,
        },
        call.id.clone(),
        call.arguments.clone(),
    ))
}
fn state(s: &str) -> Result<OpState, Status> {
    match s {
        "intended" => Ok(OpState::Intended),
        "dispatching" => Ok(OpState::Dispatching),
        "succeeded" => Ok(OpState::Succeeded),
        "failed" => Ok(OpState::Failed),
        "unknown" => Ok(OpState::Unknown),
        _ => Err(changed()),
    }
}
async fn record(
    tx: &mut Tx<'_>,
    run: &str,
    op: &str,
) -> Result<Option<host::OperationRecord>, Status> {
    let row=sqlx::query("SELECT i.argument_digest,i.approval_id,s.version,s.state,f.artifact_id,f.result_digest,
        CASE WHEN d.decision='reject' THEN 'rejected' WHEN d.expires_at<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint THEN 'expired'
             WHEN d.decision='approve' THEN 'approved' ELSE 'pending' END AS decision
        FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id) LEFT JOIN cloud_operation_decisions d USING(operation_id)
        LEFT JOIN cloud_operation_results f USING(operation_id) WHERE i.run_id=$1 AND i.operation_id=$2 FOR UPDATE OF s")
        .bind(run).bind(op).fetch_optional(&mut **tx).await.map_err(db_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(host::OperationRecord {
        operation_id: op.into(),
        run_id: run.into(),
        version: count(row.try_get("version").map_err(db_error)?)?,
        state: state(row.try_get("state").map_err(db_error)?)? as i32,
        argument_digest: row.try_get("argument_digest").map_err(db_error)?,
        result_artifact_id: row
            .try_get::<Option<String>, _>("artifact_id")
            .map_err(db_error)?
            .unwrap_or_default(),
        approval_id: row.try_get("approval_id").map_err(db_error)?,
        approval_state: row.try_get("decision").map_err(db_error)?,
        result_digest: row
            .try_get::<Option<String>, _>("result_digest")
            .map_err(db_error)?
            .unwrap_or_default(),
    }))
}
struct Proposal {
    source: Source,
    intent: host::IntentRequest,
    call: String,
    args: Value,
}
async fn discover(
    pool: &Pool,
    cert: &str,
    a: &host::RunAuthority,
) -> Result<Option<Proposal>, Status> {
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    current(&mut tx, cert, a, false).await?;
    let source = source(&mut tx, &a.run_id).await?;
    live_at_commit(&mut tx, &a.run_id).await?;
    tx.commit().await.map_err(db_error)?;
    let Some(source) = source else {
        return Ok(None);
    };
    let bytes = pool
        .reader
        .as_ref()
        .ok_or_else(changed)?
        .cloud_text_input(&source.root, &source.key, source.size, &source.sha)
        .await
        .map_err(|_| changed())?;
    let (intent, call, args) = parse_call(a, &source, &bytes)?;
    Ok(Some(Proposal {
        source,
        intent,
        call,
        args,
    }))
}
async fn check_source(tx: &mut Tx<'_>, a: &host::RunAuthority, p: &Proposal) -> Result<(), Status> {
    if source(tx, &a.run_id).await?.as_ref() != Some(&p.source) {
        return Err(changed());
    }
    Ok(())
}
pub(super) async fn pending(
    pool: &Pool,
    cert: &str,
    a: &host::RunAuthority,
) -> Result<host::PendingWork, Status> {
    let Some(p) = discover(pool, cert, a).await? else {
        return Ok(host::PendingWork { tools: vec![] });
    };
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    current(&mut tx, cert, a, false).await?;
    check_source(&mut tx, a, &p).await?;
    let existing = record(&mut tx, &a.run_id, &p.intent.operation_id).await?;
    live_at_commit(&mut tx, &a.run_id).await?;
    tx.commit().await.map_err(db_error)?;
    Ok(host::PendingWork {
        tools: vec![host::ProposedTool {
            intent: Some(p.intent),
            arguments_json: serde_json::to_vec(&p.args).map_err(|_| changed())?,
            record: existing,
        }],
    })
}
pub(super) async fn append(
    pool: &Pool,
    cert: &str,
    intent: host::IntentRequest,
) -> Result<host::OperationRecord, Status> {
    let a = intent.authority.as_ref().ok_or_else(changed)?;
    let p = discover(pool, cert, a).await?.ok_or_else(changed)?;
    if intent != p.intent {
        return Err(changed());
    }
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    current(&mut tx, cert, a, false).await?;
    check_source(&mut tx, a, &p).await?;
    let approved_hash = digest(
        "cloud.tool-review.v1",
        &(
            &a.run_id,
            &intent.operation_id,
            &p.source.commit,
            &p.source.environment,
            &p.source.capability,
            &p.source.tool_version,
            &p.source.policy,
            &intent.argument_digest,
        ),
    )?;
    let inserted=sqlx::query("INSERT INTO cloud_operation_intents(operation_id,run_id,call_id,commit_id,capability_id,approval_id,tool_version,policy_version,
        environment_id,args,argument_digest,approval_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT(operation_id) DO NOTHING")
        .bind(&intent.operation_id).bind(&a.run_id).bind(p.call).bind(&intent.response_commit_id).bind(&intent.capability_snapshot_id).bind(&intent.approval_id)
        .bind(&intent.tool_version).bind(&intent.policy_version).bind(&intent.execution_environment_id).bind(p.args).bind(&intent.argument_digest).bind(approved_hash)
        .execute(&mut *tx).await.map_err(db_error)?;
    if inserted.rows_affected() == 1 {
        sqlx::query("INSERT INTO cloud_operation_state(operation_id,state) VALUES($1,'intended')")
            .bind(&intent.operation_id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        leases::event(&mut tx, &a.run_id, "tool_intent_recorded").await?;
    }
    let result = record(&mut tx, &a.run_id, &intent.operation_id)
        .await?
        .ok_or_else(changed)?;
    live_at_commit(&mut tx, &a.run_id).await?;
    tx.commit().await.map_err(db_error)?;
    Ok(result)
}
pub(super) async fn dispatch(
    pool: &Pool,
    cert: &str,
    r: host::DispatchRequest,
) -> Result<host::DispatchPermit, Status> {
    let a = r.authority.as_ref().ok_or_else(changed)?;
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    let (row, _) = current(&mut tx, cert, a, false).await?;
    source(&mut tx, &a.run_id).await?.ok_or_else(changed)?;
    let op = record(&mut tx, &a.run_id, &r.operation_id)
        .await?
        .ok_or_else(changed)?;
    if op.version != r.expected_version
        || op.state != OpState::Intended as i32
        || op.argument_digest != r.argument_digest
        || op.approval_state != "approved"
    {
        return Err(changed());
    }
    let expires:i64=sqlx::query_scalar("SELECT LEAST(d.expires_at,$2,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+10)
        FROM cloud_operation_decisions d JOIN cloud_operation_intents i USING(operation_id)
        WHERE d.operation_id=$1 AND d.user_id=$3 AND d.decision='approve' AND d.approval_digest=i.approval_digest")
        .bind(&r.operation_id).bind(seconds(a.expires_at)?).bind(row.try_get::<String,_>("user_id").map_err(db_error)?)
        .fetch_optional(&mut *tx).await.map_err(db_error)?.ok_or_else(changed)?;
    if expires <= now(&mut tx).await? {
        return Err(changed());
    }
    let permit = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO cloud_dispatch_permits(operation_id,permit_id,execution_epoch,allocation_id,expires_at) VALUES($1,$2,$3,$4,$5)")
        .bind(&r.operation_id).bind(&permit).bind(i64::try_from(a.run_epoch).map_err(|_|changed())?)
        .bind(row.try_get::<String,_>("allocation_id").map_err(db_error)?).bind(expires).execute(&mut *tx).await.map_err(db_error)?;
    sqlx::query("UPDATE cloud_operation_state SET state='dispatching',version=version+1 WHERE operation_id=$1 AND state='intended'")
        .bind(&r.operation_id).execute(&mut *tx).await.map_err(db_error)?;
    let actual = record(&mut tx, &a.run_id, &r.operation_id)
        .await?
        .ok_or_else(changed)?;
    leases::event(&mut tx, &a.run_id, "tool_dispatch_recorded").await?;
    live_at_commit(&mut tx, &a.run_id).await?;
    if expires <= now(&mut tx).await? {
        return Err(changed());
    }
    tx.commit().await.map_err(db_error)?;
    Ok(host::DispatchPermit {
        authority: Some(a.clone()),
        operation: Some(actual),
        expires_at: Some(stamp(expires)),
        permit_id: permit,
    })
}
pub(super) async fn commit(
    pool: &Pool,
    cert: &str,
    r: host::ResultRequest,
) -> Result<host::OperationRecord, Status> {
    let a = r.authority.as_ref().ok_or_else(changed)?;
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    let (parent, _) = current(&mut tx, cert, a, false).await?;
    let source = source(&mut tx, &a.run_id).await?.ok_or_else(changed)?;
    let record_before = record(&mut tx, &a.run_id, &r.operation_id)
        .await?
        .ok_or_else(changed)?;
    let p=sqlx::query("SELECT p.expires_at,p.permit_id,i.args FROM cloud_dispatch_permits p JOIN cloud_operation_intents i USING(operation_id)
        WHERE p.operation_id=$1 AND p.execution_epoch=$2 AND p.allocation_id=$3")
        .bind(&r.operation_id).bind(i64::try_from(a.run_epoch).map_err(|_|changed())?).bind(parent.try_get::<String,_>("allocation_id").map_err(db_error)?)
        .fetch_optional(&mut *tx).await.map_err(db_error)?.ok_or_else(changed)?;
    if p.try_get::<String, _>("permit_id").map_err(db_error)? != r.permit_id
        || record_before.version != r.expected_version
        || record_before.state != OpState::Dispatching as i32
    {
        return Err(changed());
    }
    let (phase, result, artifact) = match OpState::try_from(r.result_state) {
        Ok(OpState::Succeeded) => {
            if r.result_json.len() > 16384
                || r.result_json.is_empty()
                || p.try_get::<i64, _>("expires_at").map_err(db_error)? <= now(&mut tx).await?
            {
                return Err(changed());
            }
            let v = orange_protocol::canonical::parse_tool_arguments(
                std::str::from_utf8(&r.result_json).map_err(|_| changed())?,
            )
            .map_err(|_| changed())?;
            crate::cloud_tools::verify_output(&v).map_err(|_| changed())?;
            if digest("cloud.directory-result.v1", &v)? != r.result_digest {
                return Err(changed());
            }
            let entries = json!([{"name":"input.txt","kind":"file","bytes":parent.try_get::<i64,_>("size_bytes").map_err(db_error)?.to_string()}]);
            let snapshot = digest(
                "cloud.workspace-directory.v1",
                &(&source.environment, &source.capability, &entries),
            )?;
            if v["kind"] != crate::cloud_tools::NAME
                || v["data"]
                    != json!({"path":".","snapshot_digest":snapshot,"entries":entries,"next_cursor":null,"omitted_unsupported":false})
            {
                return Err(changed());
            }
            let id = digest(
                "cloud.result-artifact.v1",
                &(&a.run_id, &r.operation_id, &r.result_digest),
            )?;
            if !r.result_artifact_id.is_empty() && r.result_artifact_id != id {
                return Err(changed());
            }
            ("succeeded", Some(v), Some(id))
        }
        Ok(OpState::Failed | OpState::Unknown)
            if r.result_json.is_empty()
                && r.result_artifact_id.is_empty()
                && valid_digest(&r.result_digest) =>
        {
            (
                if r.result_state == OpState::Failed as i32 {
                    "failed"
                } else {
                    "unknown"
                },
                None,
                None,
            )
        }
        _ => return Err(changed()),
    };
    sqlx::query("INSERT INTO cloud_operation_results(operation_id,state,artifact_id,result,result_digest) VALUES($1,$2,$3,$4,$5)")
        .bind(&r.operation_id).bind(phase).bind(artifact).bind(result).bind(&r.result_digest).execute(&mut *tx).await.map_err(db_error)?;
    sqlx::query(
        "UPDATE cloud_operation_state SET state=$2,version=version+1 WHERE operation_id=$1",
    )
    .bind(&r.operation_id)
    .bind(phase)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    let out = record(&mut tx, &a.run_id, &r.operation_id)
        .await?
        .ok_or_else(changed)?;
    live_at_commit(&mut tx, &a.run_id).await?;
    if phase == "succeeded"
        && p.try_get::<i64, _>("expires_at").map_err(db_error)? <= now(&mut tx).await?
    {
        return Err(changed());
    }
    let continuing: bool = phase == "succeeded"
        && sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_continuation_options WHERE run_id=$1)",
        )
        .bind(&a.run_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
    let next = if continuing {
        "waiting_approval"
    } else if phase == "unknown" {
        "recovery_required"
    } else {
        "paused"
    };
    let rev: i64 = sqlx::query_scalar(
        "UPDATE cloud_runs SET status=$2,version=version+1 WHERE run_id=$1 RETURNING version",
    )
    .bind(&a.run_id)
    .bind(next)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
        .bind(&a.run_id)
        .bind(rev)
        .bind(if continuing {
            "tool_result_ready_for_continuation".to_owned()
        } else {
            format!("tool_{phase}")
        })
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    if !continuing {
        sqlx::query("UPDATE cloud_workspaces SET status='draining',version=version+1,epoch=epoch+1,lease_until=NULL WHERE run_id=$1 AND status='ready'")
            .bind(&a.run_id).execute(&mut *tx).await.map_err(db_error)?;
    }
    tx.commit().await.map_err(db_error)?;
    Ok(out)
}

pub(super) async fn lookup(
    pool: &Pool,
    cert: &str,
    r: host::OperationLookup,
) -> Result<host::OperationRecord, Status> {
    let mut tx = pool.db.begin().await.map_err(db_error)?;
    leases::peer(&mut tx, &r.host_instance_id, cert, false).await?;
    let valid=sqlx::query("SELECT p.permit_id FROM cloud_dispatch_permits p JOIN cloud_operation_intents i USING(operation_id)
        JOIN cloud_workspace_claims c ON c.allocation_id=p.allocation_id JOIN cloud_workspaces w ON w.workspace_id=c.workspace_id AND w.run_id=i.run_id
        WHERE p.permit_id=$1 AND p.operation_id=$2 AND p.allocation_id=$3 AND p.execution_epoch=$4
          AND c.host_instance_id=$5 AND w.workspace_id=$6 AND w.run_id=$7")
        .bind(r.permit_id).bind(&r.operation_id).bind(r.allocation_id).bind(i64::try_from(r.run_epoch).map_err(|_|changed())?)
        .bind(r.host_instance_id).bind(r.workspace_id).bind(&r.run_id).fetch_optional(&mut *tx).await.map_err(db_error)?;
    if valid.is_none() {
        return Err(changed());
    }
    let result = record(&mut tx, &r.run_id, &r.operation_id)
        .await?
        .ok_or_else(changed)?;
    tx.commit().await.map_err(db_error)?;
    Ok(result)
}
