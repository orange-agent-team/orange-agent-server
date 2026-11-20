use super::*;
use sqlx::{Postgres, Transaction, postgres::PgRow};
type Tx<'a> = Transaction<'a, Postgres>;
fn conflict() -> Status {
    Status::failed_precondition("workspace authority or state changed")
}
async fn peer(tx: &mut Tx<'_>, host: &str, cert: &str, require_live: bool) -> Result<(), Status> {
    let row=sqlx::query("SELECT host_instance_id FROM cloud_pool_hosts WHERE host_instance_id=$1 AND certificate_sha256=$2
        AND enabled AND credential_expires_at>clock_timestamp() AND (NOT $3 OR live_until>clock_timestamp()) FOR SHARE")
        .bind(host).bind(cert).bind(require_live).fetch_optional(&mut **tx).await.map_err(db_error)?;
    if row.is_none() {
        return Err(Status::permission_denied("host admission denied"));
    }
    Ok(())
}
async fn allocation(
    tx: &mut Tx<'_>,
    host: &str,
    workspace: &str,
    run: &str,
) -> Result<PgRow, Status> {
    sqlx::query("SELECT w.version,w.status,w.epoch,r.status AS run_state,
        FLOOR(EXTRACT(EPOCH FROM w.lease_until))::bigint AS until,w.previous_lease_until AS previous,
        FLOOR(EXTRACT(EPOCH FROM LEAST(w.expires_at,h.credential_expires_at,h.live_until,clock_timestamp()+interval '30 seconds')))::bigint AS ceiling,
        FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AS now,r.user_id,r.owner_id,r.root_budget_id,r.input_artifact_id,r.input_version_id
        FROM cloud_workspaces w JOIN cloud_runs r USING(run_id) JOIN cloud_pool_hosts h USING(host_instance_id)
        WHERE w.workspace_id=$1 AND w.run_id=$2 AND w.host_instance_id=$3 FOR UPDATE OF r,w")
        .bind(workspace).bind(run).bind(host).fetch_optional(&mut **tx).await.map_err(db_error)?.ok_or_else(||Status::not_found("workspace not assigned"))
}
async fn input_live(tx: &mut Tx<'_>, row: &PgRow) -> Result<(), Status> {
    let ok=sqlx::query("SELECT a.artifact_id FROM artifacts a JOIN users u ON u.user_id=a.creator_id AND u.personal_owner_id=a.owner_id
        JOIN owners o ON o.owner_id=a.owner_id JOIN budget_roots b ON b.owner_id=a.owner_id
        WHERE a.artifact_id=$1 AND a.version_id=$2 AND a.owner_id=$3 AND a.creator_id=$4 AND b.root_budget_id=$5
          AND a.resource_id IS NULL AND a.state='available' AND a.media_type='text/plain' AND u.active AND o.state='active' AND b.state='active'
        FOR SHARE OF a,u,o,b").bind(row.try_get::<String,_>("input_artifact_id").map_err(db_error)?)
        .bind(row.try_get::<String,_>("input_version_id").map_err(db_error)?).bind(row.try_get::<String,_>("owner_id").map_err(db_error)?)
        .bind(row.try_get::<String,_>("user_id").map_err(db_error)?).bind(row.try_get::<String,_>("root_budget_id").map_err(db_error)?)
        .fetch_optional(&mut **tx).await.map_err(db_error)?;
    if ok.is_none() {
        return Err(conflict());
    }
    Ok(())
}
async fn event(tx: &mut Tx<'_>, run: &str, kind: &str) -> Result<(), Status> {
    let version: i64 = sqlx::query_scalar(
        "UPDATE cloud_runs SET version=version+1 WHERE run_id=$1 RETURNING version",
    )
    .bind(run)
    .fetch_one(&mut **tx)
    .await
    .map_err(db_error)?;
    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
        .bind(run)
        .bind(version)
        .bind(kind)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    Ok(())
}
async fn confirm_live(tx: &mut Tx<'_>, workspace: &str) -> Result<(), Status> {
    let live:bool=sqlx::query_scalar("SELECT w.lease_until>clock_timestamp() AND w.expires_at>clock_timestamp()
        AND h.enabled AND h.live_until>clock_timestamp() AND h.credential_expires_at>clock_timestamp()
        FROM cloud_workspaces w JOIN cloud_pool_hosts h USING(host_instance_id) WHERE w.workspace_id=$1")
        .bind(workspace).fetch_one(&mut **tx).await.map_err(db_error)?;
    if !live {
        return Err(conflict());
    }
    Ok(())
}
pub(super) async fn claim(
    db: &PgPool,
    cert: &str,
    c: host::ClaimWorkspaceRequest,
) -> Result<host::WorkspaceLease, Status> {
    if [
        &c.host_instance_id,
        &c.workspace_id,
        &c.run_id,
        &c.command_id,
    ]
    .iter()
    .any(|v| !valid_id(v))
        || c.expected_workspace_version == 0
    {
        return Err(Status::invalid_argument("invalid claim"));
    }
    let hash = orange_protocol::request_digest(
        "cloud.provision-claim.v1",
        &(
            &c.host_instance_id,
            &c.run_id,
            &c.workspace_id,
            c.expected_workspace_version,
        ),
    )
    .map_err(|_| Status::invalid_argument("invalid claim"))?;
    let mut tx = db.begin().await.map_err(db_error)?;
    peer(&mut tx, &c.host_instance_id, cert, true).await?;
    let row = allocation(&mut tx, &c.host_instance_id, &c.workspace_id, &c.run_id).await?;
    if row.try_get::<String, _>("run_state").map_err(db_error)? != "queued" {
        return Err(conflict());
    }
    input_live(&mut tx, &row).await?;
    let at: i64 = row.try_get("now").map_err(db_error)?;
    let ceiling: i64 = row.try_get("ceiling").map_err(db_error)?;
    if ceiling <= at {
        return Err(conflict());
    }
    let prior: Option<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT allocation_id,request_digest,epoch,initial_until FROM cloud_workspace_claims
        WHERE host_instance_id=$1 AND command_id=$2",
    )
    .bind(&c.host_instance_id)
    .bind(&c.command_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    let (allocation_id, epoch, until) = if let Some((id, old, epoch, until)) = prior {
        if old != hash
            || row.try_get::<i64, _>("epoch").map_err(db_error)? != epoch
            || row.try_get::<String, _>("status").map_err(db_error)? != "preparing"
            || row.try_get::<Option<i64>, _>("until").map_err(db_error)? != Some(until)
            || until <= at
        {
            return Err(conflict());
        }
        (id, epoch, until)
    } else {
        if row.try_get::<String, _>("status").map_err(db_error)? != "requested"
            || u64::try_from(row.try_get::<i64, _>("version").map_err(db_error)?).ok()
                != Some(c.expected_workspace_version)
        {
            return Err(conflict());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let epoch:i64=sqlx::query_scalar("UPDATE cloud_workspaces SET status='preparing',version=version+1,epoch=epoch+1,lease_until=to_timestamp($2)
            WHERE workspace_id=$1 RETURNING epoch").bind(&c.workspace_id).bind(ceiling as f64).fetch_one(&mut *tx).await.map_err(db_error)?;
        sqlx::query("INSERT INTO cloud_workspace_claims(allocation_id,workspace_id,host_instance_id,command_id,request_digest,epoch,initial_until)
            VALUES($1,$2,$3,$4,$5,$6,$7)").bind(&id).bind(&c.workspace_id).bind(&c.host_instance_id).bind(c.command_id).bind(hash)
            .bind(epoch).bind(ceiling).execute(&mut *tx).await.map_err(db_error)?;
        event(&mut tx, &c.run_id, "workspace_preparation_claimed").await?;
        (id, epoch, ceiling)
    };
    confirm_live(&mut tx, &c.workspace_id).await?;
    tx.commit().await.map_err(db_error)?;
    Ok(host::WorkspaceLease {
        host_instance_id: c.host_instance_id,
        run_id: c.run_id,
        workspace_id: c.workspace_id,
        allocation_id,
        epoch: u64::try_from(epoch).map_err(|_| conflict())?,
        lease_until: Some(stamp(until)),
    })
}
pub(super) async fn renew(
    db: &PgPool,
    cert: &str,
    mut lease: host::WorkspaceLease,
) -> Result<host::WorkspaceLease, Status> {
    let offered = lease
        .lease_until
        .as_ref()
        .filter(|t| t.nanos == 0)
        .ok_or_else(conflict)?
        .seconds;
    let mut tx = db.begin().await.map_err(db_error)?;
    peer(&mut tx, &lease.host_instance_id, cert, true).await?;
    let row = allocation(
        &mut tx,
        &lease.host_instance_id,
        &lease.workspace_id,
        &lease.run_id,
    )
    .await?;
    let epoch = i64::try_from(lease.epoch).map_err(|_| conflict())?;
    let known:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_workspace_claims WHERE workspace_id=$1 AND allocation_id=$2 AND epoch=$3)")
        .bind(&lease.workspace_id).bind(&lease.allocation_id).bind(epoch).fetch_one(&mut *tx).await.map_err(db_error)?;
    let current = row
        .try_get::<Option<i64>, _>("until")
        .map_err(db_error)?
        .ok_or_else(conflict)?;
    let at: i64 = row.try_get("now").map_err(db_error)?;
    if !known
        || epoch != row.try_get::<i64, _>("epoch").map_err(db_error)?
        || row.try_get::<String, _>("status").map_err(db_error)? != "preparing"
        || current <= at
        || offered <= at
    {
        return Err(conflict());
    }
    input_live(&mut tx, &row).await?;
    let next = if offered == current {
        let cap: i64 = row.try_get("ceiling").map_err(db_error)?;
        if cap <= at {
            return Err(conflict());
        }
        if cap != current {
            sqlx::query("UPDATE cloud_workspaces SET previous_lease_until=$2,lease_until=to_timestamp($3) WHERE workspace_id=$1")
            .bind(&lease.workspace_id).bind(current).bind(cap as f64).execute(&mut *tx).await.map_err(db_error)?;
        }
        cap
    } else if row
        .try_get::<Option<i64>, _>("previous")
        .map_err(db_error)?
        == Some(offered)
    {
        current
    } else {
        return Err(conflict());
    };
    lease.lease_until = Some(stamp(next));
    confirm_live(&mut tx, &lease.workspace_id).await?;
    tx.commit().await.map_err(db_error)?;
    Ok(lease)
}
pub(super) async fn cleanup(
    db: &PgPool,
    cert: &str,
    c: host::WorkspaceCleanup,
) -> Result<host::CleanupReceipt, Status> {
    if !valid_digest(&c.evidence_digest) || !c.sandbox_absent || !c.cgroup_absent {
        return Err(conflict());
    }
    let mut tx = db.begin().await.map_err(db_error)?;
    peer(&mut tx, &c.host_instance_id, cert, false).await?;
    let row = allocation(&mut tx, &c.host_instance_id, &c.workspace_id, &c.run_id).await?;
    let epoch = i64::try_from(c.allocation_epoch).map_err(|_| conflict())?;
    let known:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_workspace_claims WHERE allocation_id=$1 AND workspace_id=$2 AND epoch=$3)")
        .bind(&c.allocation_id).bind(&c.workspace_id).bind(epoch).fetch_one(&mut *tx).await.map_err(db_error)?;
    if !known {
        return Err(conflict());
    }
    let old:Option<(String,String)>=sqlx::query_as("SELECT receipt_id,evidence_digest FROM cloud_workspace_cleanup_receipts WHERE workspace_id=$1")
        .bind(&c.workspace_id).fetch_optional(&mut *tx).await.map_err(db_error)?;
    let receipt_id = if let Some((receipt, proof)) = old {
        if proof != c.evidence_digest {
            return Err(conflict());
        }
        receipt
    } else {
        if row.try_get::<String, _>("status").map_err(db_error)? != "draining"
            || row.try_get::<i64, _>("epoch").map_err(db_error)? <= epoch
        {
            return Err(conflict());
        }
        let receipt = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO cloud_workspace_cleanup_receipts(workspace_id,allocation_id,evidence_digest,receipt_id) VALUES($1,$2,$3,$4)")
            .bind(&c.workspace_id).bind(c.allocation_id).bind(&c.evidence_digest).bind(&receipt).execute(&mut *tx).await.map_err(db_error)?;
        sqlx::query(
            "UPDATE cloud_workspaces SET status='deleted',version=version+1 WHERE workspace_id=$1",
        )
        .bind(&c.workspace_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        event(&mut tx, &c.run_id, "workspace_cleanup_confirmed").await?;
        receipt
    };
    tx.commit().await.map_err(db_error)?;
    Ok(host::CleanupReceipt {
        receipt_id,
        workspace_id: c.workspace_id,
        evidence_digest: c.evidence_digest,
    })
}
