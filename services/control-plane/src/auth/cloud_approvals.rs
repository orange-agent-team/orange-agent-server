//! Browser decisions use the user's own identity and the immutable operation
//! preview. A model, public Worker or platform Host cannot approve an effect.
use super::*;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;
fn db(_: sqlx::Error) -> UserApiError {
    AuthError::Unavailable.into()
}
fn conflict() -> UserApiError {
    UserApiError(StatusCode::CONFLICT, "OPERATION_NOT_CURRENT")
}
async fn target(tx: &mut Tx<'_>, run: &str, owner: &str, user: &str) -> Result<(), UserApiError> {
    if !valid_id(run) {
        return Err(AuthError::Invalid.into());
    }
    sqlx::query("SELECT r.run_id FROM cloud_runs r JOIN cloud_directory_capabilities c USING(run_id) WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 FOR UPDATE OF r")
        .bind(run).bind(owner).bind(user).fetch_optional(&mut **tx).await.map_err(db)?.ok_or(UserApiError(StatusCode::NOT_FOUND,"NOT_FOUND"))?;
    Ok(())
}
pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(run): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(&headers).await?;
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let tx = g.transaction();
    target(tx, &run, &owner, &user).await?;
    let scope=sqlx::query("SELECT c.capability_id,c.workspace_id,c.execution_digest,c.policy_version,a.state AS source_state,v.receipt_id
        FROM cloud_directory_capabilities c JOIN cloud_runs r USING(run_id) JOIN artifacts a ON a.artifact_id=r.input_artifact_id
        LEFT JOIN cloud_directory_revocations v USING(run_id) WHERE c.run_id=$1 FOR SHARE OF a")
        .bind(&run).fetch_one(&mut **tx).await.map_err(db)?;
    let revoked = scope
        .try_get::<Option<String>, _>("receipt_id")
        .map_err(db)?
        .is_some();
    let source_ok = scope.try_get::<String, _>("source_state").map_err(db)? == "available";
    let rows=sqlx::query("SELECT i.*,s.state,s.version,d.decision,d.expires_at,f.artifact_id,f.result,f.result_digest,out.state AS output_state
        FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id)
        JOIN cloud_tool_responses t ON t.run_id=i.run_id AND t.commit_id=i.commit_id JOIN artifacts out ON out.artifact_id=t.artifact_id
        LEFT JOIN cloud_operation_decisions d USING(operation_id) LEFT JOIN cloud_operation_results f USING(operation_id)
        WHERE i.run_id=$1 ORDER BY i.created_at,i.operation_id FOR SHARE OF out,s")
        .bind(&run).fetch_all(&mut **tx).await.map_err(db)?;
    let mut data = Vec::new();
    for row in rows {
        let mut v = json!({"operation_id":row.try_get::<String,_>("operation_id").map_err(db)?,"approval_id":row.try_get::<String,_>("approval_id").map_err(db)?,
            "tool_name":"list_directory","tool_version":row.try_get::<String,_>("tool_version").map_err(db)?,"argument_digest":row.try_get::<String,_>("argument_digest").map_err(db)?,
            "review_digest":row.try_get::<String,_>("approval_digest").map_err(db)?,"version":row.try_get::<i64,_>("version").map_err(db)?.to_string(),
            "state":row.try_get::<String,_>("state").map_err(db)?,"commit_id":row.try_get::<String,_>("commit_id").map_err(db)?});
        if let Some(decision) = row.try_get::<Option<String>, _>("decision").map_err(db)? {
            v["decision"] = json!(decision);
        }
        if !revoked
            && source_ok
            && row.try_get::<String, _>("output_state").map_err(db)? == "available"
        {
            v["arguments"] = row.try_get("args").map_err(db)?;
            if let Some(value) = row.try_get::<Option<Value>, _>("result").map_err(db)? {
                if value["kind"] != crate::cloud_tools::NAME {
                    return Err(AuthError::Unavailable.into());
                }
                v["result"] = value["data"].clone();
                v["result_artifact_id"] =
                    json!(row.try_get::<String, _>("artifact_id").map_err(db)?);
            }
        }
        data.push(v);
    }
    let response = json!({"run_id":run,"owner_id":owner,"user_id":user,"capability_id":scope.try_get::<String,_>("capability_id").map_err(db)?,
        "workspace_id":scope.try_get::<String,_>("workspace_id").map_err(db)?,"execution_digest":scope.try_get::<String,_>("execution_digest").map_err(db)?,
        "policy_version":scope.try_get::<String,_>("policy_version").map_err(db)?,"revoked":revoked,"source_available":source_ok,"data":data});
    g.finish().await?;
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    review_digest: String,
    expected_version: String,
    decision: String,
}
pub(super) async fn decide(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path((run, op)): Path<(String, String)>,
    body: Result<Json<Decision>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Invalid)?;
    let r = body.map_err(|_| AuthError::Invalid)?.0;
    let version = r
        .expected_version
        .parse::<i64>()
        .map_err(|_| AuthError::Invalid)?;
    if !id_ok(&op)
        || !orange_protocol::valid_digest(&r.review_digest)
        || version < 1
        || version.to_string() != r.expected_version
        || !matches!(r.decision.as_str(), "approve" | "reject")
    {
        return Err(AuthError::Invalid.into());
    }
    let owner = guard.view().personal_owner_id.clone();
    let user = guard.view().user_id.clone();
    let tx = guard.transaction();
    target(tx, &run, &owner, &user).await?;
    let row=sqlx::query("SELECT i.approval_digest,s.state,s.version FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id)
        WHERE i.operation_id=$1 AND i.run_id=$2 FOR UPDATE OF s").bind(&op).bind(&run).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(conflict)?;
    if row.try_get::<String, _>("approval_digest").map_err(db)? != r.review_digest {
        return Err(UserApiError(
            StatusCode::PRECONDITION_FAILED,
            "REVIEW_CHANGED",
        ));
    }
    let old=sqlx::query("SELECT operation_id,request_key,decision,reviewed_version FROM cloud_operation_decisions WHERE operation_id=$1 OR (user_id=$2 AND request_key=$3)")
        .bind(&op).bind(&user).bind(key).fetch_all(&mut **tx).await.map_err(db)?;
    if let [prior] = old.as_slice() {
        if prior.try_get::<String, _>("operation_id").map_err(db)? != op
            || prior.try_get::<String, _>("request_key").map_err(db)? != key
            || prior.try_get::<String, _>("decision").map_err(db)? != r.decision
            || prior.try_get::<i64, _>("reviewed_version").map_err(db)? != version
        {
            return Err(conflict());
        }
    } else {
        if !old.is_empty()
            || row.try_get::<String, _>("state").map_err(db)? != "intended"
            || row.try_get::<i64, _>("version").map_err(db)? != version
        {
            return Err(conflict());
        }
        crate::cloud_model::live(tx, &run, &owner, &user)
            .await
            .map_err(|_| conflict())?;
        let inserted=sqlx::query("INSERT INTO cloud_operation_decisions(operation_id,user_id,decision,approval_digest,expires_at,request_key,reviewed_version)
            SELECT $1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM LEAST(w.expires_at,clock_timestamp()+interval '5 minutes')))::bigint,$5,$6
            FROM cloud_workspaces w JOIN cloud_tool_responses t USING(run_id) JOIN artifacts a ON a.artifact_id=t.artifact_id AND a.version_id=t.version_id
            WHERE w.run_id=$7 AND a.state='available' AND orange_artifact_content_live(a.artifact_id) AND NOT EXISTS(SELECT 1 FROM cloud_directory_revocations WHERE run_id=w.run_id) FOR SHARE OF a")
            .bind(&op).bind(&user).bind(&r.decision).bind(&r.review_digest).bind(key).bind(version).bind(&run).execute(&mut **tx).await.map_err(db)?;
        if inserted.rows_affected() != 1 {
            return Err(conflict());
        }
        sqlx::query(
            "UPDATE cloud_operation_state SET version=version+1,state=$2 WHERE operation_id=$1",
        )
        .bind(&op)
        .bind(if r.decision == "reject" {
            "failed"
        } else {
            "intended"
        })
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        if r.decision == "reject" {
            stop_effects(tx, &run, "user_rejected_tool").await?;
        } else {
            let v: i64 = sqlx::query_scalar(
                "UPDATE cloud_runs SET version=version+1 WHERE run_id=$1 RETURNING version",
            )
            .bind(&run)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
            sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,'user_approved_tool')")
                .bind(&run).bind(v).execute(&mut **tx).await.map_err(db)?;
            crate::cloud_model::live(tx, &run, &owner, &user)
                .await
                .map_err(|_| conflict())?;
        }
    }
    guard.finish().await?;
    Ok(Json(
        json!({"run_id":run,"operation_id":op,"decision":r.decision,"review_digest":r.review_digest,"user_id":user,"owner_id":owner}),
    ))
}
fn id_ok(op: &str) -> bool {
    orange_protocol::valid_digest(op)
}
async fn stop_effects(tx: &mut Tx<'_>, run: &str, kind: &str) -> Result<(), UserApiError> {
    crate::cloud_tools::fence(tx, run, kind).await.map_err(db)?;
    crate::store::stop_cloud_job(tx, run).await?;
    let state:Option<i64>=sqlx::query_scalar("UPDATE cloud_runs SET status='paused',version=version+1 WHERE run_id=$1 AND status IN ('queued','running','waiting_approval') RETURNING version")
        .bind(run).fetch_optional(&mut **tx).await.map_err(db)?;
    if let Some(v) = state {
        sqlx::query("UPDATE cloud_workspaces SET status=CASE WHEN status='requested' AND epoch=0 THEN 'deleted' ELSE 'draining' END,
            epoch=epoch+1,version=version+1,lease_until=NULL WHERE run_id=$1 AND status IN ('requested','preparing','ready','running')").bind(run).execute(&mut **tx).await.map_err(db)?;
        sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
            .bind(run)
            .bind(v)
            .bind(kind)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
    }
    Ok(())
}
pub(super) async fn revoke(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(run): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    let owner = guard.view().personal_owner_id.clone();
    let user = guard.view().user_id.clone();
    let tx = guard.transaction();
    target(tx, &run, &owner, &user).await?;
    let new=sqlx::query("INSERT INTO cloud_directory_revocations(run_id,receipt_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(&run).bind(uuid::Uuid::new_v4().to_string()).bind(&user).execute(&mut **tx).await.map_err(db)?;
    if new.rows_affected() == 1 {
        stop_effects(tx, &run, "directory_capability_revoked").await?;
    }
    let receipt: String = sqlx::query_scalar(
        "SELECT receipt_id FROM cloud_directory_revocations WHERE run_id=$1 AND user_id=$2",
    )
    .bind(&run)
    .bind(&user)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    guard.finish().await?;
    Ok(Json(
        json!({"run_id":run,"user_id":user,"owner_id":owner,"receipt_id":receipt,"revoked":true}),
    ))
}
