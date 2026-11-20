//! Platform connector execution is separate from public Worker inference and guest workspace dispatch.
use super::connector_access::{db, denied, guard, key};
use super::*;
use crate::{artifacts::ArtifactStore, auth::users::UserSessionGuard, hosted_connectors as tools};
use orange_protocol::model::ResponseCandidate;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
#[derive(Clone)]
struct Api {
    identity: Arc<UserIdentity>,
    artifacts: ArtifactStore,
}
pub(super) fn router(identity: UserIdentity, artifacts: ArtifactStore) -> Router {
    Router::new()
        .route(
            "/api/v1/agent/runs/{run}/model/connector",
            get(status).post(decide),
        )
        .layer(DefaultBodyLimit::max(16384))
        .layer(middleware::from_fn(private_headers))
        .with_state(Api {
            identity: Arc::new(identity),
            artifacts,
        })
}
pub(super) async fn available(
    i: &UserIdentity,
    owner: &str,
    family: &str,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<bool, UserApiError> {
    let digests: Vec<&str> = match family {
        "postgres" => i
            .pg_sources
            .0
            .values()
            .filter(|s| s.owner_id == owner)
            .map(|s| s.digest.as_str())
            .collect(),
        "s3" => i
            .s3_sources
            .0
            .values()
            .filter(|s| s.owner == owner)
            .map(|s| s.digest.as_str())
            .collect(),
        _ => return Ok(false),
    };
    sqlx::query_scalar("SELECT EXISTS(SELECT FROM unnest($1::text[]) AS active(digest) WHERE NOT EXISTS(SELECT FROM connector_credential_faults f WHERE f.source_digest=active.digest))")
        .bind(digests).fetch_one(&mut **tx).await.map_err(db)
}
pub(super) async fn catalog(
    State(i): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let mut g = i.authenticate(&headers).await?;
    let owner = g.view().personal_owner_id.clone();
    let mut values = Vec::new();
    for kind in ["postgres", "s3"] {
        if available(&i, &owner, kind, g.transaction()).await? {
            for (family, name, _) in tools::TOOLS {
                if family == kind {
                    values.push(tools::public(name).map_err(|_| AuthError::Unavailable)?);
                }
            }
        }
    }
    let user = g.finish().await?.user_id;
    Ok(Json(
        json!({"user_id":user,"owner_id":owner,"tools":values}),
    ))
}
struct Proposal {
    job: String,
    commit: String,
    name: String,
    pin: String,
    family: String,
    call_id: String,
    args: Value,
    operation: String,
    arg_digest: String,
}
fn conflict() -> UserApiError {
    UserApiError(StatusCode::CONFLICT, "CONNECTOR_OPERATION_NOT_CURRENT")
}
async fn proposal(api: &Api, headers: &HeaderMap, run: &str) -> Result<Proposal, UserApiError> {
    let (mut g, _) = guard(&api.identity, headers, run, false, false).await?;
    let owner = g.view().personal_owner_id.clone();
    let row=sqlx::query("SELECT m.job_id,m.commit_id,m.artifact_id,m.version_id,p.tool_name,p.connector_kind,p.execution_digest FROM cloud_connector_model_commits m JOIN cloud_connector_plans p USING(run_id) JOIN job_response_commits c ON c.job_id=m.job_id AND c.commit_id=m.commit_id AND c.artifact_id=m.artifact_id AND c.version_id=m.version_id AND c.response_digest=m.response_digest JOIN job_settlements s ON s.job_id=m.job_id JOIN cloud_runs r USING(run_id) JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id WHERE m.run_id=$1 AND src.state='available' AND r.status<>'cancelled' FOR SHARE OF src").bind(run).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(conflict)?;
    g.finish().await?;
    let actor = api.identity.artifact_actor(headers, false).await?;
    let job: String = row.get("job_id");
    let commit: String = row.get("commit_id");
    let response: ResponseCandidate = api
        .artifacts
        .committed_connector_response(&actor, &owner, run, &job, &commit)
        .await
        .map_err(|_| denied())?;
    let [call] = response.tool_calls.as_slice() else {
        return Err(conflict());
    };
    let name: String = row.get("tool_name");
    let pin: String = row.get("execution_digest");
    let family: String = row.get("connector_kind");
    if response.finish_reason != "tool_calls" || tools::family(&name) != Some(family.as_str()) {
        return Err(conflict());
    }
    let args = tools::action(call, &name, &pin).map_err(|_| conflict())?;
    let arg_digest = orange_protocol::request_digest("connector.call.arguments.v1", &args)
        .map_err(|_| conflict())?;
    let operation = orange_protocol::request_digest(
        "connector.call.operation.v1",
        &(run, &job, &commit, &call.id, &pin, &arg_digest),
    )
    .map_err(|_| conflict())?;
    Ok(Proposal {
        job,
        commit,
        name,
        pin,
        family,
        call_id: call.id.clone(),
        args,
        operation,
        arg_digest,
    })
}
async fn grant_authority(
    i: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    p: &Proposal,
    grant: &str,
) -> Result<(), UserApiError> {
    if p.family == "s3" {
        super::s3_connectors::live(i, g, run, grant).await?;
    } else {
        let (_, views) = super::pg_connectors::authority(i, g, run, grant).await?;
        if p.args["view_id"]
            .as_str()
            .is_some_and(|id| !views.contains(id))
        {
            return Err(denied());
        }
    }
    super::connector_access::released(g, &p.family, grant, &p.name).await?;
    Ok(())
}
async fn decision_status(
    api: &Api,
    h: &HeaderMap,
    run: &str,
    p: &Proposal,
) -> Result<Value, UserApiError> {
    let (mut g, readable) = guard(&api.identity, h, run, false, false).await?;
    // A lost post-approval task has no authority to dispatch a second time.
    sqlx::query("INSERT INTO cloud_connector_effect_receipts(run_id,state) SELECT run_id,'unknown' FROM cloud_connector_decisions WHERE run_id=$1 AND decision='approve' AND expires_at<=clock_timestamp() ON CONFLICT DO NOTHING").bind(run).execute(&mut **g.transaction()).await.map_err(db)?;
    let row=sqlx::query("SELECT d.decision,d.grant_id,d.operation_id,r.state,body.result,r.result_digest FROM cloud_connector_decisions d LEFT JOIN cloud_connector_effect_receipts r USING(run_id) LEFT JOIN cloud_connector_result_content body ON body.run_id=d.run_id AND body.retain_until>clock_timestamp() WHERE d.run_id=$1").bind(run).fetch_optional(&mut **g.transaction()).await.map_err(db)?;
    let mut value = json!({"run_id":run,"job_id":p.job,"commit_id":p.commit,"tool":p.name,"connector":p.family,"execution_digest":p.pin,"operation_id":p.operation,"call_id":p.call_id,"argument_digest":p.arg_digest,"arguments":p.args,"state":"waiting_approval"});
    if let Some(row) = row {
        if row.get::<String, _>("operation_id") != p.operation {
            return Err(conflict());
        }
        value["state"] = json!(
            row.get::<Option<String>, _>("state")
                .as_deref()
                .unwrap_or("dispatching")
        );
        if !readable {
            value
                .as_object_mut()
                .ok_or_else(conflict)?
                .remove("arguments");
        } else if let Some(grant) = row.get::<Option<String>, _>("grant_id") {
            match grant_authority(&api.identity, &mut g, run, p, &grant).await {
                Ok(()) => {
                    value["grant_id"] = json!(grant);
                    if let Some(body) = row.get::<Option<Value>, _>("result") {
                        let expected =
                            orange_protocol::request_digest("connector.tool.result.v1", &body)
                                .map_err(|_| conflict())?;
                        if row.get::<Option<String>, _>("result_digest").as_deref()
                            != Some(&expected)
                        {
                            return Err(conflict());
                        }
                        value["result"] = body;
                    }
                }
                Err(UserApiError(
                    StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE,
                    _,
                )) => {
                    value
                        .as_object_mut()
                        .ok_or_else(conflict)?
                        .remove("arguments");
                    value["source_unavailable"] = json!(true);
                }
                Err(err) => return Err(err),
            }
        }
    }
    g.finish().await?;
    Ok(value)
}
async fn status(
    State(api): State<Api>,
    Path(run): Path<String>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let p = proposal(&api, &h, &run).await?;
    Ok(Json(decision_status(&api, &h, &run, &p).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    operation_id: String,
    argument_digest: String,
    decision: String,
    grant_id: Option<String>,
}
pub(super) struct ReadPermit<'a> {
    proposal: &'a Proposal,
}
impl ReadPermit<'_> {
    pub(super) fn key(&self) -> String {
        format!("connector-effect/{}", self.proposal.operation)
    }
    pub(super) async fn check(
        &self,
        g: &mut UserSessionGuard,
        run: &str,
        grant: &str,
    ) -> Result<(), UserApiError> {
        let p = self.proposal;
        sqlx::query("SELECT d.run_id FROM cloud_connector_decisions d JOIN cloud_connector_plans plan USING(run_id)
            JOIN cloud_connector_model_commits m USING(run_id) JOIN cloud_runs r USING(run_id) JOIN cloud_workspaces w USING(run_id)
            JOIN cloud_pool_hosts h USING(host_instance_id) JOIN budget_roots b ON b.root_budget_id=r.root_budget_id AND b.owner_id=r.owner_id
            JOIN artifacts model_response ON model_response.artifact_id=m.artifact_id AND model_response.version_id=m.version_id
            JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id
            WHERE d.run_id=$1 AND d.grant_id=$2 AND d.operation_id=$3 AND d.argument_digest=$4 AND d.decision='approve' AND d.expires_at>clock_timestamp()
              AND m.commit_id=$5 AND plan.execution_digest=$6 AND r.status='waiting_approval' AND w.status='ready' AND w.expires_at>clock_timestamp()
              AND w.lease_until>clock_timestamp() AND h.enabled AND h.live_until>clock_timestamp() AND h.credential_expires_at>clock_timestamp()
              AND b.state='active' AND src.state='available' AND model_response.state='available' AND NOT EXISTS(SELECT FROM cloud_connector_effect_receipts out WHERE out.run_id=d.run_id)
            FOR SHARE OF h,w,model_response,src,b")
            .bind(run).bind(grant).bind(&p.operation).bind(&p.arg_digest).bind(&p.commit).bind(&p.pin)
            .fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(conflict)?;
        Ok(())
    }
}
async fn read_source(
    api: &Api,
    h: &HeaderMap,
    run: &str,
    grant: &str,
    p: &Proposal,
) -> Result<Value, UserApiError> {
    let permit = ReadPermit { proposal: p };
    let i = api.identity.clone();
    if p.family == "s3" {
        let action = serde_json::from_value(p.args.clone()).map_err(|_| AuthError::Invalid)?;
        Ok(super::s3_connectors::perform_checked(
            i,
            run.to_owned(),
            grant.to_owned(),
            h.clone(),
            action,
            Some(&permit),
        )
        .await?
        .0)
    } else {
        let action = serde_json::from_value(p.args.clone()).map_err(|_| AuthError::Invalid)?;
        Ok(super::pg_connectors::perform_checked(
            i,
            run.to_owned(),
            grant.to_owned(),
            h.clone(),
            action,
            Some(&permit),
        )
        .await?
        .0)
    }
}
async fn decide(
    State(api): State<Api>,
    Path(run): Path<String>,
    h: HeaderMap,
    body: Result<Json<Review>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(review) = body.map_err(|_| AuthError::Invalid)?;
    let req = key(&h)?;
    let p = proposal(&api, &h, &run).await?;
    if p.operation != review.operation_id || p.arg_digest != review.argument_digest {
        return Err(conflict());
    }
    let approved = review.decision == "approve";
    if !(approved || review.decision == "reject") || approved != review.grant_id.is_some() {
        return Err(AuthError::Invalid.into());
    }
    let (mut g, readable) = guard(&api.identity, &h, &run, true, false).await?;
    if let Some(prior) = sqlx::query(
        "SELECT request_key,decision,grant_id FROM cloud_connector_decisions WHERE run_id=$1",
    )
    .bind(&run)
    .fetch_optional(&mut **g.transaction())
    .await
    .map_err(db)?
    {
        if prior.get::<String, _>("request_key") != req
            || prior.get::<String, _>("decision") != review.decision
            || prior.get::<Option<String>, _>("grant_id") != review.grant_id
        {
            return Err(conflict());
        }
        g.finish().await?;
        return Ok(Json(decision_status(&api, &h, &run, &p).await?));
    }
    if !readable {
        return Err(denied());
    }
    if let Some(id) = &review.grant_id {
        grant_authority(&api.identity, &mut g, &run, &p, id).await?;
    }
    sqlx::query("SELECT r.run_id FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_pool_hosts h USING(host_instance_id) WHERE r.run_id=$1 AND r.status='waiting_approval' AND w.status='ready' AND w.expires_at>clock_timestamp() AND w.lease_until>clock_timestamp() AND h.enabled AND h.live_until>clock_timestamp() FOR SHARE OF w,h")
        .bind(&run).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(conflict)?;
    sqlx::query("INSERT INTO cloud_connector_decisions(run_id,operation_id,call_id,argument_digest,decision,grant_id,request_key,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+interval '35 seconds')").bind(&run).bind(&p.operation).bind(&p.call_id).bind(&p.arg_digest).bind(&review.decision).bind(&review.grant_id).bind(req).execute(&mut **g.transaction()).await.map_err(db)?;
    if !approved {
        sqlx::query(
            "INSERT INTO cloud_connector_effect_receipts(run_id,state) VALUES($1,'rejected')",
        )
        .bind(&run)
        .execute(&mut **g.transaction())
        .await
        .map_err(db)?;
    }
    g.finish().await?;
    if let Some(grant) = review.grant_id {
        let fetched = read_source(&api, &h, &run, &grant, &p).await;
        let (mut g, _) = guard(&api.identity, &h, &run, true, true).await?;
        let (state, body, digest) = match fetched {
            Ok(body) => {
                grant_authority(&api.identity, &mut g, &run, &p, &grant).await?;
                tools::check_result(&p.name, &body).map_err(|_| conflict())?;
                let digest = orange_protocol::request_digest("connector.tool.result.v1", &body)
                    .map_err(|_| conflict())?;
                ("succeeded", Some(body), Some(digest))
            }
            Err(UserApiError(
                StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
                | StatusCode::BAD_GATEWAY,
                _,
            )) => ("unknown", None, None),
            Err(_) => ("failed", None, None),
        };
        let inserted=sqlx::query("INSERT INTO cloud_connector_effect_receipts(run_id,state,result_digest) SELECT run_id,$2,$3 FROM cloud_connector_decisions WHERE run_id=$1 AND expires_at>clock_timestamp() ON CONFLICT DO NOTHING")
            .bind(&run).bind(state).bind(&digest).execute(&mut **g.transaction()).await.map_err(db)?;
        if let (1, Some(body), Some(digest)) = (inserted.rows_affected(), body, digest) {
            let kept=sqlx::query("INSERT INTO cloud_connector_result_content(run_id,digest,result,retain_until) SELECT run_id,$2,$3,retain_until FROM cloud_connector_retention WHERE run_id=$1")
                .bind(&run).bind(digest).bind(body).execute(&mut **g.transaction()).await.map_err(db)?;
            if kept.rows_affected() != 1 {
                return Err(denied());
            }
        }
        g.finish().await?;
    }
    Ok(Json(decision_status(&api, &h, &run, &p).await?))
}
