//! E07 Run execution authority. A workspace provisioning lease, public Worker
//! grant, customer cookie, and model response are never Run execution tokens.
use super::*;
use host::run_store_server::RunStore;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
type Tx<'a> = Transaction<'a, Postgres>;
#[path = "cloud_run_operations.rs"]
mod operations;
fn changed() -> Status {
    Status::failed_precondition("run authority or operation is no longer current")
}
fn seconds(v: Option<prost_types::Timestamp>) -> Result<i64, Status> {
    v.filter(|s| s.nanos == 0 && s.seconds > 0)
        .map(|s| s.seconds)
        .ok_or_else(changed)
}
fn count(v: i64) -> Result<u64, Status> {
    u64::try_from(v).map_err(|_| changed())
}
fn authority(row: &PgRow, lease: &PgRow) -> Result<host::RunAuthority, Status> {
    Ok(host::RunAuthority {
        run_id: row.try_get("run_id").map_err(db_error)?,
        host_instance_id: row.try_get("host_instance_id").map_err(db_error)?,
        owner_scope_id: row.try_get("owner_id").map_err(db_error)?,
        root_budget_id: row.try_get("root_budget_id").map_err(db_error)?,
        run_epoch: count(lease.try_get("epoch").map_err(db_error)?)?,
        run_version: count(lease.try_get("issued_version").map_err(db_error)?)?,
        expires_at: Some(stamp(lease.try_get("lease_until").map_err(db_error)?)),
    })
}
async fn parent(tx: &mut Tx<'_>, cert: &str, instance: &str, id: &str) -> Result<PgRow, Status> {
    leases::peer(tx, instance, cert, true).await?;
    let row=sqlx::query("SELECT r.run_id,r.version,r.status,r.owner_id,r.user_id,r.root_budget_id,w.workspace_id,w.host_instance_id,w.epoch AS workspace_epoch,
        m.allocation_id,p.execution_protocol,p.invocation_id,p.input_version_id,a.size_bytes,
        FLOOR(EXTRACT(EPOCH FROM LEAST(w.lease_until,w.expires_at,h.live_until,h.credential_expires_at,clock_timestamp()+interval '25 seconds')))::bigint AS ceiling
        FROM cloud_runs r JOIN cloud_workspaces w USING(run_id,owner_id) JOIN cloud_pool_hosts h USING(host_instance_id)
        JOIN cloud_model_plans p USING(run_id,owner_id) JOIN cloud_model_proposals m USING(run_id) JOIN artifacts a ON a.artifact_id=r.input_artifact_id
        WHERE r.run_id=$1 AND w.host_instance_id=$2 AND h.runstore_version=1 AND p.execution_protocol=1 AND w.status='ready'
        AND r.status IN ('queued','waiting_approval','running') FOR UPDATE OF r,w").bind(id).bind(instance)
        .fetch_optional(&mut **tx).await.map_err(db_error)?.ok_or_else(changed)?;
    crate::cloud_model::workspace_context(
        tx,
        id,
        row.try_get("owner_id").map_err(db_error)?,
        row.try_get("user_id").map_err(db_error)?,
    )
    .await
    .map_err(|e| match e {
        crate::artifacts::ArtifactError::Database(e) => db_error(e),
        _ => changed(),
    })?;
    Ok(row)
}
async fn now(tx: &mut Tx<'_>) -> Result<i64, Status> {
    sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
        .fetch_one(&mut **tx)
        .await
        .map_err(db_error)
}
async fn current(
    tx: &mut Tx<'_>,
    cert: &str,
    a: &host::RunAuthority,
    allow_previous: bool,
) -> Result<(PgRow, PgRow), Status> {
    if !valid_id(&a.run_id) || !valid_id(&a.host_instance_id) {
        return Err(changed());
    }
    let parent = parent(tx, cert, &a.host_instance_id, &a.run_id).await?;
    let lease = sqlx::query(
        "SELECT * FROM cloud_run_leases WHERE run_id=$1 AND allocation_id=$2 FOR UPDATE",
    )
    .bind(&a.run_id)
    .bind(
        parent
            .try_get::<String, _>("allocation_id")
            .map_err(db_error)?,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?
    .ok_or_else(changed)?;
    let expected = authority(&parent, &lease)?;
    let at = now(tx).await?;
    if expected.run_epoch != a.run_epoch
        || expected.owner_scope_id != a.owner_scope_id
        || expected.root_budget_id != a.root_budget_id
        || expected.host_instance_id != a.host_instance_id
        || seconds(expected.expires_at)? <= at
        || seconds(a.expires_at)? <= at
    {
        return Err(changed());
    }
    let exact = expected.run_version == a.run_version && expected.expires_at == a.expires_at;
    let prior = allow_previous
        && lease
            .try_get::<Option<i64>, _>("previous_until")
            .map_err(db_error)?
            == Some(seconds(a.expires_at)?)
        && lease
            .try_get::<Option<i64>, _>("previous_version")
            .map_err(db_error)?
            == i64::try_from(a.run_version).ok();
    if !exact && !prior {
        return Err(changed());
    }
    Ok((parent, lease))
}
async fn live_at_commit(tx: &mut Tx<'_>, run: &str) -> Result<(), Status> {
    let live:bool=sqlx::query_scalar("SELECT l.lease_until>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AND w.lease_until>clock_timestamp()
        AND w.expires_at>clock_timestamp() AND h.live_until>clock_timestamp() AND h.credential_expires_at>clock_timestamp() AND h.enabled
        FROM cloud_run_leases l JOIN cloud_workspaces w USING(run_id,host_instance_id) JOIN cloud_pool_hosts h USING(host_instance_id) WHERE l.run_id=$1")
        .bind(run).fetch_one(&mut **tx).await.map_err(db_error)?;
    if !live {
        return Err(changed());
    }
    Ok(())
}
#[tonic::async_trait]
impl RunStore for Pool {
    async fn claim(
        &self,
        request: Request<host::ClaimRequest>,
    ) -> Result<Response<host::RunAuthority>, Status> {
        let cert = leaf(&request)?;
        let r = request.into_inner();
        if [&r.run_id, &r.host_instance_id, &r.command_id]
            .iter()
            .any(|v| !valid_id(v))
            || r.expected_run_version == 0
        {
            return Err(Status::invalid_argument("invalid run claim"));
        }
        let mut tx = self.db.begin().await.map_err(db_error)?;
        let parent = parent(&mut tx, &cert, &r.host_instance_id, &r.run_id).await?;
        let old=sqlx::query("SELECT * FROM cloud_run_leases WHERE run_id=$1 OR (host_instance_id=$2 AND command_id=$3) FOR UPDATE")
            .bind(&r.run_id).bind(&r.host_instance_id).bind(&r.command_id).fetch_all(&mut *tx).await.map_err(db_error)?;
        let expected = i64::try_from(r.expected_run_version).map_err(|_| changed())?;
        let lease = if let [old] = old.as_slice() {
            if old.try_get::<String, _>("run_id").map_err(db_error)? != r.run_id
                || old.try_get::<String, _>("command_id").map_err(db_error)? != r.command_id
                || old.try_get::<i64, _>("request_version").map_err(db_error)? != expected
                || old
                    .try_get::<String, _>("allocation_id")
                    .map_err(db_error)?
                    != parent
                        .try_get::<String, _>("allocation_id")
                        .map_err(db_error)?
            {
                return Err(changed());
            }
            authority(&parent, old)?
        } else {
            if !old.is_empty()
                || parent.try_get::<String, _>("status").map_err(db_error)? != "queued"
                || expected != parent.try_get::<i64, _>("version").map_err(db_error)?
            {
                return Err(changed());
            }
            let version:i64=sqlx::query_scalar("UPDATE cloud_runs SET status='waiting_approval',version=version+1 WHERE run_id=$1 RETURNING version")
                .bind(&r.run_id).fetch_one(&mut *tx).await.map_err(db_error)?;
            let l=sqlx::query("INSERT INTO cloud_run_leases(run_id,allocation_id,host_instance_id,command_id,request_version,issued_version,lease_until)
                VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING *").bind(&r.run_id).bind(parent.try_get::<String,_>("allocation_id").map_err(db_error)?)
                .bind(&r.host_instance_id).bind(&r.command_id).bind(expected).bind(version).bind(parent.try_get::<i64,_>("ceiling").map_err(db_error)?)
                .fetch_one(&mut *tx).await.map_err(db_error)?;
            sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,'agent_execution_claimed')")
                .bind(&r.run_id).bind(version).execute(&mut *tx).await.map_err(db_error)?;
            authority(&parent, &l)?
        };
        live_at_commit(&mut tx, &r.run_id).await?;
        tx.commit().await.map_err(db_error)?;
        Ok(Response::new(lease))
    }
    async fn renew(
        &self,
        request: Request<host::RunAuthority>,
    ) -> Result<Response<host::RunAuthority>, Status> {
        let cert = leaf(&request)?;
        let token = request.into_inner();
        let mut tx = self.db.begin().await.map_err(db_error)?;
        let (row, old) = current(&mut tx, &cert, &token, true).await?;
        let current = authority(&row, &old)?;
        let limit: i64 = row.try_get("ceiling").map_err(db_error)?;
        let version: i64 = row.try_get("version").map_err(db_error)?;
        let result = if current == token
            && (seconds(current.expires_at)? != limit || current.run_version != count(version)?)
        {
            let next=sqlx::query("UPDATE cloud_run_leases SET previous_until=lease_until,previous_version=issued_version,lease_until=$2,issued_version=$3
                WHERE run_id=$1 RETURNING *").bind(&token.run_id).bind(limit).bind(version).fetch_one(&mut *tx).await.map_err(db_error)?;
            authority(&row, &next)?
        } else {
            current
        };
        live_at_commit(&mut tx, &token.run_id).await?;
        tx.commit().await.map_err(db_error)?;
        Ok(Response::new(result))
    }
    async fn pending_operations(
        &self,
        req: Request<host::RunAuthority>,
    ) -> Result<Response<host::PendingWork>, Status> {
        let cert = leaf(&req)?;
        Ok(Response::new(
            operations::pending(self, &cert, &req.into_inner()).await?,
        ))
    }
    async fn append_intent(
        &self,
        req: Request<host::IntentRequest>,
    ) -> Result<Response<host::OperationRecord>, Status> {
        let cert = leaf(&req)?;
        Ok(Response::new(
            operations::append(self, &cert, req.into_inner()).await?,
        ))
    }
    async fn acquire_dispatch(
        &self,
        req: Request<host::DispatchRequest>,
    ) -> Result<Response<host::DispatchPermit>, Status> {
        let cert = leaf(&req)?;
        Ok(Response::new(
            operations::dispatch(self, &cert, req.into_inner()).await?,
        ))
    }
    async fn get_operation_receipt(
        &self,
        req: Request<host::OperationLookup>,
    ) -> Result<Response<host::OperationRecord>, Status> {
        let cert = leaf(&req)?;
        Ok(Response::new(
            operations::lookup(self, &cert, req.into_inner()).await?,
        ))
    }
    async fn commit_result(
        &self,
        req: Request<host::ResultRequest>,
    ) -> Result<Response<host::OperationRecord>, Status> {
        let cert = leaf(&req)?;
        Ok(Response::new(
            operations::commit(self, &cert, req.into_inner()).await?,
        ))
    }
}
