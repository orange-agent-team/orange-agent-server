//! Personal cloud Run allocation. A reservation is not a running sandbox or a
//! model charge. A claimed workspace keeps its quota until confirmed cleanup.
use super::*;
use crate::auth::users::UserSessionGuard;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};

pub(crate) const PROFILE: &str = "dev-gvisor-linux-arm64-v1";
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    root_budget_id: String,
    input_artifact_id: String,
    input_version_id: String,
    resource_profile_id: String,
    workspace_ttl_seconds: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "model_choice"
    )]
    model: Option<crate::cloud_model::Selection>,
}
fn model_choice<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> Result<Option<crate::cloud_model::Selection>, D::Error> {
    crate::cloud_model::Selection::deserialize(input).map(Some)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    before: Option<String>,
    limit: Option<u16>,
}
fn db(_: sqlx::Error) -> UserApiError {
    AuthError::Unavailable.into()
}
fn missing() -> UserApiError {
    UserApiError(StatusCode::NOT_FOUND, "NOT_FOUND")
}
fn busy() -> UserApiError {
    UserApiError(StatusCode::SERVICE_UNAVAILABLE, "HOST_POOL_UNAVAILABLE")
}
fn input_valid(c: &Create) -> bool {
    c.model.as_ref().is_none_or(|m| m.valid())
        && valid_id(&c.root_budget_id)
        && valid_id(&c.input_artifact_id)
        && valid_id(&c.input_version_id)
        && c.resource_profile_id == PROFILE
        && (60..=3600).contains(&c.workspace_ttl_seconds)
}
macro_rules! select_run {($filter:literal)=>{concat!("SELECT r.run_id,r.owner_id,r.user_id,r.status AS run_state,r.version AS run_version,r.root_budget_id,
 r.input_artifact_id,r.input_version_id,FLOOR(EXTRACT(EPOCH FROM r.created_at))::bigint AS created,
 w.workspace_id,w.status AS workspace_state,w.version AS workspace_version,w.profile_id,FLOOR(EXTRACT(EPOCH FROM w.expires_at))::bigint AS expires
 FROM cloud_runs r JOIN cloud_workspaces w ON w.run_id=r.run_id AND w.owner_id=r.owner_id
 WHERE r.owner_id=$1 AND r.user_id=$2 ",$filter)}}
fn view(row: PgRow) -> Result<Value, UserApiError> {
    Ok(
        json!({"run_id":row.try_get::<String,_>("run_id").map_err(db)?,"owner_id":row.try_get::<String,_>("owner_id").map_err(db)?,
        "user_id":row.try_get::<String,_>("user_id").map_err(db)?,"status":row.try_get::<String,_>("run_state").map_err(db)?,
        "version":row.try_get::<i64,_>("run_version").map_err(db)?.to_string(),"root_budget_id":row.try_get::<String,_>("root_budget_id").map_err(db)?,
        "input_artifact_id":row.try_get::<String,_>("input_artifact_id").map_err(db)?,"input_version_id":row.try_get::<String,_>("input_version_id").map_err(db)?,
        "created_at":timestamp(row.try_get("created").map_err(db)?)?,
        "workspace":{"workspace_id":row.try_get::<String,_>("workspace_id").map_err(db)?,"status":row.try_get::<String,_>("workspace_state").map_err(db)?,
        "version":row.try_get::<i64,_>("workspace_version").map_err(db)?.to_string(),"resource_profile_id":row.try_get::<String,_>("profile_id").map_err(db)?,
        "expires_at":timestamp(row.try_get("expires").map_err(db)?)?}}),
    )
}
async fn one(g: &mut UserSessionGuard, id: &str) -> Result<Value, UserApiError> {
    if !valid_id(id) {
        return Err(AuthError::Invalid.into());
    }
    let row = sqlx::query(select_run!(" AND r.run_id=$3"))
        .bind(g.view().personal_owner_id.clone())
        .bind(g.view().user_id.clone())
        .bind(id)
        .fetch_optional(&mut **g.transaction())
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
    view(row)
}
pub(super) async fn get_one(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let v = one(&mut guard, &id).await?;
    guard.finish().await?;
    Ok(Json(v))
}
pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Query(q) = query.map_err(|_| AuthError::Invalid)?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) || q.before.as_deref().is_some_and(|id| !valid_id(id)) {
        return Err(AuthError::Invalid.into());
    }
    let mut guard = identity.authenticate(&headers).await?;
    let owner = guard.view().personal_owner_id.clone();
    let user = guard.view().user_id.clone();
    let tx = guard.transaction();
    let before = if let Some(id) = q.before {
        Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT sequence FROM cloud_runs WHERE run_id=$1 AND owner_id=$2 AND user_id=$3",
            )
            .bind(id)
            .bind(&owner)
            .bind(&user)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?
            .ok_or_else(missing)?,
        )
    } else {
        None
    };
    let rows = sqlx::query(select_run!(
        " AND ($3::bigint IS NULL OR r.sequence<$3) ORDER BY r.sequence DESC LIMIT $4"
    ))
    .bind(&owner)
    .bind(&user)
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut **tx)
    .await
    .map_err(db)?;
    let more = rows.len() > usize::from(limit);
    let entries = rows
        .into_iter()
        .take(usize::from(limit))
        .map(view)
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = json!({"data":entries,"owner_id":owner,"user_id":user});
    if more {
        result["next_before"] = result["data"][usize::from(limit) - 1]["run_id"].clone();
    }
    guard.finish().await?;
    Ok(Json(result))
}
async fn event(
    tx: &mut Transaction<'_, Postgres>,
    run: &str,
    version: i64,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
        .bind(run)
        .bind(version)
        .bind(kind)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Serialize create and withdrawal across sessions for one personal owner.
async fn lock_quota(tx: &mut Transaction<'_, Postgres>, owner: &str) -> Result<i32, UserApiError> {
    sqlx::query("INSERT INTO cloud_workspace_quotas(owner_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(owner)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query_scalar("SELECT max_active FROM cloud_workspace_quotas WHERE owner_id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)
}
pub(super) async fn create(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    body: Result<Json<Create>, JsonRejection>,
) -> Result<Response, UserApiError> {
    let mut g = identity.authenticate(&headers).await?;
    check_write(&headers, &identity.config.user_origin, &g.view().csrf_token)?;
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Invalid)?;
    let c = body.map_err(|_| AuthError::Invalid)?.0;
    if !input_valid(&c) || identity.config.environment != "dev" {
        return Err(AuthError::Invalid.into());
    }
    let digest = orange_protocol::request_digest("cloud.personal-run.v1", &c)
        .map_err(|_| AuthError::Invalid)?;
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let tx = g.transaction();
    let max = lock_quota(tx, &owner).await?;
    let withdrawn: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cloud_run_withdrawals WHERE owner_id=$1 AND request_key=$2)",
    )
    .bind(&owner)
    .bind(key)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if withdrawn {
        return Err(UserApiError(StatusCode::GONE, "RUN_REQUEST_WITHDRAWN"));
    }
    let prior:Option<(String,String)>=sqlx::query_as("SELECT run_id,request_digest FROM cloud_runs WHERE owner_id=$1 AND user_id=$2 AND request_key=$3")
        .bind(&owner).bind(&user).bind(key).fetch_optional(&mut **tx).await.map_err(db)?;
    let run = if let Some((id, old)) = prior {
        if old != digest {
            return Err(UserApiError(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"));
        }
        id
    } else {
        if let Some(m) = &c.model {
            crate::store::lock_model_invocation(tx, &owner, &m.invocation_id).await?;
        }
        let budget=sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE")
            .bind(&c.root_budget_id).bind(&owner).fetch_optional(&mut **tx).await.map_err(db)?;
        let file=sqlx::query("SELECT artifact_id FROM artifacts WHERE artifact_id=$1 AND version_id=$2 AND owner_id=$3 AND creator_id=$4
            AND resource_id IS NULL AND state='available' AND media_type=$5 AND size_bytes BETWEEN 1 AND 262144 FOR SHARE")
            .bind(&c.input_artifact_id).bind(&c.input_version_id).bind(&owner).bind(&user).bind(if c.model.is_some(){"application/json"}else{"text/plain"}).fetch_optional(&mut **tx).await.map_err(db)?;
        if budget.is_none() || file.is_none() {
            return Err(missing());
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_workspaces WHERE owner_id=$1 AND status!='deleted'",
        )
        .bind(&owner)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        if count >= i64::from(max) {
            return Err(UserApiError(
                StatusCode::CONFLICT,
                "WORKSPACE_QUOTA_EXCEEDED",
            ));
        }
        let host:String=sqlx::query_scalar("SELECT host_instance_id FROM cloud_pool_hosts h WHERE enabled AND input_protocol_version=1 AND accepting AND profile_id=$1 AND (NOT $2 OR runstore_version=1)
            AND live_until>clock_timestamp() AND credential_expires_at>clock_timestamp()
            AND (SELECT count(*) FROM cloud_workspaces w WHERE w.host_instance_id=h.host_instance_id AND w.status!='deleted')<max_workspaces
            ORDER BY host_instance_id LIMIT 1 FOR UPDATE SKIP LOCKED")
            .bind(PROFILE).bind(c.model.is_some()).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(busy)?;
        // Re-read capacity after acquiring the host lock: the selector may have
        // used an earlier read-committed snapshot than a concurrent admission.
        let available:bool=sqlx::query_scalar("SELECT enabled AND input_protocol_version=1 AND accepting AND live_until>clock_timestamp() AND credential_expires_at>clock_timestamp() AND
            (SELECT count(*) FROM cloud_workspaces w WHERE w.host_instance_id=h.host_instance_id AND w.status!='deleted')<max_workspaces
            FROM cloud_pool_hosts h WHERE host_instance_id=$1").bind(&host).fetch_one(&mut **tx).await.map_err(db)?;
        if !available {
            return Err(busy());
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO cloud_runs(run_id,owner_id,user_id,request_key,request_digest,root_budget_id,input_artifact_id,input_version_id,status)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,'queued')").bind(&id).bind(&owner).bind(&user).bind(key).bind(digest).bind(&c.root_budget_id)
            .bind(&c.input_artifact_id).bind(&c.input_version_id).execute(&mut **tx).await.map_err(db)?;
        sqlx::query("INSERT INTO cloud_workspaces(workspace_id,run_id,owner_id,host_instance_id,profile_id,status,expires_at)
            VALUES($1,$2,$3,$4,$5,'requested',clock_timestamp()+make_interval(secs=>$6))")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&id).bind(&owner).bind(host).bind(PROFILE).bind(f64::from(c.workspace_ttl_seconds))
            .execute(&mut **tx).await.map_err(db)?;
        if let Some(model) = &c.model {
            crate::cloud_model::register(tx, &id, &owner, &c.input_version_id, model).await?;
        }
        let still_live:bool=sqlx::query_scalar("SELECT enabled AND input_protocol_version=1 AND accepting AND live_until>clock_timestamp() AND credential_expires_at>clock_timestamp()
            FROM cloud_pool_hosts h JOIN cloud_workspaces w USING(host_instance_id) WHERE w.run_id=$1")
            .bind(&id).fetch_one(&mut **tx).await.map_err(db)?;
        if !still_live {
            return Err(busy());
        }
        event(tx, &id, 1, "workspace_requested").await.map_err(db)?;
        id
    };
    let result = one(&mut g, &run).await?;
    g.finish().await?;
    Ok((StatusCode::CREATED, Json(result)).into_response())
}
pub(super) async fn cancel(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(&headers).await?;
    check_write(&headers, &identity.config.user_origin, &g.view().csrf_token)?;
    if !valid_id(&id) {
        return Err(AuthError::Invalid.into());
    }
    cancel_locked(&mut g, &id).await?;
    let v = one(&mut g, &id).await?;
    g.finish().await?;
    Ok(Json(v))
}
async fn cancel_locked(g: &mut UserSessionGuard, id: &str) -> Result<(), UserApiError> {
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let tx = g.transaction();
    let row=sqlx::query("SELECT r.status,r.version,w.status AS workspace_state,w.epoch FROM cloud_runs r JOIN cloud_workspaces w USING(run_id)
        WHERE r.run_id=$1 AND r.owner_id=$2 AND r.user_id=$3 FOR UPDATE OF r,w").bind(id).bind(owner).bind(user)
        .fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(missing)?;
    let status: String = row.try_get("status").map_err(db)?;
    if status != "cancelled" {
        if !matches!(
            status.as_str(),
            "queued" | "running" | "paused" | "waiting_approval" | "recovery_required"
        ) {
            return Err(UserApiError(StatusCode::CONFLICT, "RUN_TERMINAL"));
        }
        let state: String = row.try_get("workspace_state").map_err(db)?;
        let unclaimed = state == "requested" && row.try_get::<i64, _>("epoch").map_err(db)? == 0;
        crate::cloud_tools::fence(tx, id, "user_cancel")
            .await
            .map_err(db)?;
        crate::store::stop_cloud_job(tx, id).await?;
        let next:i64=sqlx::query_scalar("UPDATE cloud_runs SET status='cancelled',version=version+1 WHERE run_id=$1 RETURNING version")
            .bind(id).fetch_one(&mut **tx).await.map_err(db)?;
        let kind = match state.as_str() {
            "deleted" => "cancelled_after_cleanup",
            "draining" | "quarantined" => "cancelled_awaiting_cleanup",
            _ => {
                sqlx::query("UPDATE cloud_workspaces SET status=$2,epoch=epoch+1,lease_until=NULL,version=version+1 WHERE run_id=$1")
                    .bind(id).bind(if unclaimed{"deleted"}else{"draining"}).execute(&mut **tx).await.map_err(db)?;
                if unclaimed {
                    "cancelled_before_host_claim"
                } else {
                    "cancelled_awaiting_cleanup"
                }
            }
        };
        event(tx, id, next, kind).await.map_err(db)?;
    }
    Ok(())
}
pub(crate) async fn expire_one(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT r.run_id,w.epoch,w.status FROM cloud_runs r JOIN cloud_workspaces w USING(run_id) JOIN cloud_pool_hosts h USING(host_instance_id)
        WHERE (NOT h.enabled OR h.credential_expires_at<=clock_timestamp() OR w.expires_at<=clock_timestamp() OR w.lease_until<=clock_timestamp() OR EXISTS(SELECT 1 FROM cloud_run_leases l WHERE l.run_id=r.run_id AND l.lease_until<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)
          OR EXISTS(SELECT 1 FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id) JOIN cloud_operation_decisions d USING(operation_id)
            WHERE i.run_id=r.run_id AND s.state='intended' AND d.decision='approve' AND d.expires_at<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)) AND w.status IN ('requested','preparing','ready','running')
        ORDER BY w.expires_at,r.run_id LIMIT 1 FOR UPDATE OF r,w SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(false);
    };
    let run: String = row.try_get("run_id")?;
    let unclaimed =
        row.try_get::<i64, _>("epoch")? == 0 && row.try_get::<String, _>("status")? == "requested";
    crate::cloud_tools::fence(&mut tx, &run, "workspace_expiry").await?;
    crate::store::stop_cloud_job(&mut tx, &run)
        .await
        .map_err(|_| sqlx::Error::Protocol("linked cloud job stop unresolved".into()))?;
    let v: i64 = sqlx::query_scalar(
        "UPDATE cloud_runs SET status=$2,version=version+1 WHERE run_id=$1 RETURNING version",
    )
    .bind(&run)
    .bind(if unclaimed {
        "cancelled"
    } else {
        "recovery_required"
    })
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE cloud_workspaces SET status=$2,version=version+1,epoch=epoch+1,lease_until=NULL WHERE run_id=$1")
        .bind(&run).bind(if unclaimed{"deleted"}else{"draining"}).execute(&mut *tx).await?;
    event(
        &mut tx,
        &run,
        v,
        if unclaimed {
            "expired_before_host_claim"
        } else {
            "expired_awaiting_cleanup"
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub(super) async fn capacity(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(&headers).await?;
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let row=sqlx::query("SELECT coalesce((SELECT max_active FROM cloud_workspace_quotas WHERE owner_id=$1),2) AS quota,
        (SELECT count(*) FROM cloud_workspaces WHERE owner_id=$1 AND status!='deleted') AS occupied,
        EXISTS(SELECT 1 FROM cloud_pool_hosts h WHERE h.enabled AND h.input_protocol_version=1 AND h.accepting AND h.profile_id=$2
            AND h.live_until>clock_timestamp() AND h.credential_expires_at>clock_timestamp()
            AND (SELECT count(*) FROM cloud_workspaces w WHERE w.host_instance_id=h.host_instance_id AND w.status!='deleted')<h.max_workspaces) AS pool")
        .bind(&owner).bind(PROFILE).fetch_one(&mut **g.transaction()).await.map_err(db)?;
    let max: i32 = row.try_get("quota").map_err(db)?;
    let occupied: i64 = row.try_get("occupied").map_err(db)?;
    let pool: bool = row.try_get("pool").map_err(db)?;
    let result = json!({"user_id":user,"owner_id":owner,"resource_profile_id":PROFILE,"max_active":max,"occupied":occupied,
                      "can_reserve":pool&&occupied<i64::from(max),"pool_available":pool});
    g.finish().await?;
    Ok(Json(result))
}
pub(super) async fn lookup(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    if !valid_id(&key) {
        return Err(AuthError::Invalid.into());
    }
    let mut g = identity.authenticate(&headers).await?;
    let row = sqlx::query(select_run!(" AND r.request_key=$3"))
        .bind(g.view().personal_owner_id.clone())
        .bind(g.view().user_id.clone())
        .bind(key)
        .fetch_optional(&mut **g.transaction())
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
    let value = view(row)?;
    g.finish().await?;
    Ok(Json(value))
}

pub(super) async fn withdraw(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(&headers).await?;
    check_write(&headers, &identity.config.user_origin, &g.view().csrf_token)?;
    if !valid_id(&key) {
        return Err(AuthError::Invalid.into());
    }
    let owner = g.view().personal_owner_id.clone();
    let user = g.view().user_id.clone();
    let tx = g.transaction();
    lock_quota(tx, &owner).await?;
    sqlx::query("INSERT INTO cloud_run_withdrawals(owner_id,user_id,request_key,receipt_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(&owner).bind(&user).bind(&key).bind(uuid::Uuid::new_v4().to_string()).execute(&mut **tx).await.map_err(db)?;
    let receipt:String=sqlx::query_scalar("SELECT receipt_id FROM cloud_run_withdrawals WHERE owner_id=$1 AND user_id=$2 AND request_key=$3")
        .bind(&owner).bind(&user).bind(&key).fetch_one(&mut **tx).await.map_err(db)?;
    let run: Option<String> = sqlx::query_scalar(
        "SELECT run_id FROM cloud_runs WHERE owner_id=$1 AND user_id=$2 AND request_key=$3",
    )
    .bind(&owner)
    .bind(&user)
    .bind(&key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let mut result =
        json!({"owner_id":owner,"user_id":user,"request_key":key,"receipt_id":receipt});
    if let Some(id) = run {
        let state: String =
            sqlx::query_scalar("SELECT status FROM cloud_runs WHERE run_id=$1 FOR UPDATE")
                .bind(&id)
                .fetch_one(&mut **g.transaction())
                .await
                .map_err(db)?;
        if !matches!(state.as_str(), "failed" | "completed") {
            cancel_locked(&mut g, &id).await?;
        }
        result["run"] = one(&mut g, &id).await?;
    }
    g.finish().await?;
    Ok(Json(result))
}

pub(super) async fn tools(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let guard = identity.authenticate(&headers).await?;
    let owner = guard.view().personal_owner_id.clone();
    let user = guard.view().user_id.clone();
    let tool = crate::cloud_tools::public().map_err(|_| AuthError::Unavailable)?;
    guard.finish().await?;
    Ok(Json(
        json!({"user_id":user,"owner_id":owner,"tools":[tool]}),
    ))
}
