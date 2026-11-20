use super::*;
use sqlx::{Postgres, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;
fn changed() -> Status {
    Status::failed_precondition("workspace input authority changed")
}

#[derive(PartialEq, Eq)]
struct Binding {
    artifact: String,
    version: String,
    root: String,
    key: String,
    size: u64,
    sha: String,
    attestation: String,
    model: Option<crate::cloud_model::Selection>,
}
async fn binding(
    tx: &mut Tx<'_>,
    cert: &str,
    lease: &host::WorkspaceLease,
) -> Result<Binding, Status> {
    leases::peer(tx, &lease.host_instance_id, cert, true).await?;
    let row = leases::allocation(
        tx,
        &lease.host_instance_id,
        &lease.workspace_id,
        &lease.run_id,
    )
    .await?;
    let at: i64 = row.try_get("now").map_err(db_error)?;
    let expiry = lease
        .lease_until
        .as_ref()
        .filter(|t| t.nanos == 0 && t.seconds > at)
        .ok_or_else(changed)?
        .seconds;
    if row.try_get::<Option<i64>, _>("until").map_err(db_error)? != Some(expiry)
        || i64::try_from(lease.epoch).ok() != Some(row.try_get("epoch").map_err(db_error)?)
        || !matches!(
            row.try_get::<String, _>("status")
                .map_err(db_error)?
                .as_str(),
            "preparing" | "ready"
        )
        || row.try_get::<String, _>("run_state").map_err(db_error)? != "queued"
    {
        return Err(changed());
    }
    leases::input_live(tx, &row).await?;
    let version: String = row.try_get("input_version_id").map_err(db_error)?;
    let artifact: String = row.try_get("input_artifact_id").map_err(db_error)?;
    let source=sqlx::query("SELECT a.root_id,t.published_key,a.size_bytes,a.sha256,h.attestation_digest FROM cloud_workspace_claims c
        JOIN cloud_pool_hosts h USING(host_instance_id) CROSS JOIN artifacts a JOIN artifact_transfers t ON t.transfer_id=a.published_transfer_id
        WHERE c.allocation_id=$1 AND c.workspace_id=$2 AND c.host_instance_id=$3 AND c.epoch=$4 AND h.input_protocol_version=1
          AND a.artifact_id=$5 AND a.version_id=$6 AND t.artifact_id=a.artifact_id AND t.state='attached' AND NOT t.published_deleted
        FOR SHARE OF a,t,h").bind(&lease.allocation_id).bind(&lease.workspace_id).bind(&lease.host_instance_id).bind(i64::try_from(lease.epoch).map_err(|_|changed())?)
        .bind(&artifact).bind(&version).fetch_optional(&mut **tx).await.map_err(db_error)?.ok_or_else(changed)?;
    let plan = crate::cloud_model::selection(tx, &lease.run_id)
        .await
        .map_err(db_error)?;
    Ok(Binding {
        model: plan,
        artifact,
        version,
        root: source.try_get("root_id").map_err(db_error)?,
        key: source.try_get("published_key").map_err(db_error)?,
        size: u64::try_from(source.try_get::<i64, _>("size_bytes").map_err(db_error)?)
            .map_err(|_| changed())?,
        sha: source.try_get("sha256").map_err(db_error)?,
        attestation: source.try_get("attestation_digest").map_err(db_error)?,
    })
}
pub(super) async fn fetch(
    db: &PgPool,
    reader: &crate::artifacts::ArtifactReader,
    cert: &str,
    lease: host::WorkspaceLease,
) -> Result<host::WorkspaceInput, Status> {
    let mut tx = db.begin().await.map_err(db_error)?;
    let source = binding(&mut tx, cert, &lease).await?;
    leases::confirm_live(&mut tx, &lease.workspace_id).await?;
    tx.commit().await.map_err(db_error)?;
    let bytes = reader
        .cloud_text_input(&source.root, &source.key, source.size, &source.sha)
        .await
        .map_err(|e| match e {
            crate::artifacts::ArtifactError::Invalid
            | crate::artifacts::ArtifactError::Integrity => changed(),
            _ => Status::unavailable("workspace input unavailable"),
        })?;
    let projection = source
        .model
        .as_ref()
        .map(|model| model.projection(&bytes))
        .transpose()
        .map_err(|_| changed())?;
    let mut tx = db.begin().await.map_err(db_error)?;
    if source != binding(&mut tx, cert, &lease).await? {
        return Err(changed());
    }
    sqlx::query("INSERT INTO cloud_workspace_input_reads(allocation_id,receipt_id,artifact_id,version_id,sha256,model_projection_digest)
        VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(allocation_id) DO NOTHING").bind(&lease.allocation_id).bind(uuid::Uuid::new_v4().to_string())
        .bind(&source.artifact).bind(&source.version).bind(&source.sha).bind(&projection).execute(&mut *tx).await.map_err(db_error)?;
    let receipt_id=sqlx::query_scalar::<_,String>("SELECT receipt_id FROM cloud_workspace_input_reads WHERE allocation_id=$1 AND artifact_id=$2 AND version_id=$3 AND sha256=$4")
        .bind(&lease.allocation_id).bind(&source.artifact).bind(&source.version).bind(&source.sha)
        .fetch_optional(&mut *tx).await.map_err(db_error)?.ok_or_else(changed)?;
    leases::confirm_live(&mut tx, &lease.workspace_id).await?;
    tx.commit().await.map_err(db_error)?;
    Ok(host::WorkspaceInput {
        allocation_id: lease.allocation_id,
        receipt_id,
        artifact_id: source.artifact,
        version_id: source.version,
        sha256: source.sha,
        content: bytes,
    })
}
pub(super) async fn ready(
    db: &PgPool,
    cert: &str,
    r: host::WorkspaceReady,
) -> Result<host::WorkspaceReadyReceipt, Status> {
    let lease = r.lease.ok_or_else(changed)?;
    if !valid_digest(&r.materialization_digest)
        || !valid_id(&r.input_receipt_id)
        || r.sandbox_id != format!("oa-ws-{}", lease.allocation_id)
    {
        return Err(Status::invalid_argument("invalid materialization receipt"));
    }
    let mut tx = db.begin().await.map_err(db_error)?;
    let source = binding(&mut tx, cert, &lease).await?;
    if source.sha != r.input_sha256 || source.attestation != r.attestation_digest {
        return Err(changed());
    }
    let known=sqlx::query("SELECT receipt_id FROM cloud_workspace_input_reads WHERE allocation_id=$1 AND receipt_id=$2 AND version_id=$3 AND sha256=$4")
        .bind(&lease.allocation_id).bind(&r.input_receipt_id).bind(&source.version).bind(&source.sha)
        .fetch_optional(&mut *tx).await.map_err(db_error)?;
    if known.is_none() {
        return Err(changed());
    }
    let digest = orange_protocol::request_digest(
        "cloud.workspace-ready.v1",
        &(
            &lease.workspace_id,
            &lease.allocation_id,
            lease.epoch,
            &r.input_receipt_id,
            &r.input_sha256,
            &r.sandbox_id,
            &r.attestation_digest,
            &r.materialization_digest,
        ),
    )
    .map_err(|_| changed())?;
    let receipt=if let Some((id,old))=sqlx::query_as::<_,(String,String)>("SELECT receipt_id,request_digest FROM cloud_workspace_ready_receipts WHERE allocation_id=$1")
        .bind(&lease.allocation_id).fetch_optional(&mut *tx).await.map_err(db_error)?{
        if old!=digest{return Err(changed());}id
    }else {
        let id=uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO cloud_workspace_ready_receipts(allocation_id,receipt_id,input_receipt_id,sandbox_id,request_digest,materialization_digest)
            VALUES($1,$2,$3,$4,$5,$6)").bind(&lease.allocation_id).bind(&id).bind(r.input_receipt_id).bind(&r.sandbox_id).bind(digest)
            .bind(r.materialization_digest).execute(&mut *tx).await.map_err(db_error)?;
        let changed_rows=sqlx::query("UPDATE cloud_workspaces SET status='ready',version=version+1 WHERE workspace_id=$1 AND status='preparing'")
            .bind(&lease.workspace_id).execute(&mut *tx).await.map_err(db_error)?.rows_affected();
        if changed_rows!=1{return Err(changed());}
        if source.model.is_some(){
            let marked=sqlx::query("INSERT INTO cloud_model_proposals(run_id,allocation_id,input_receipt_id,request_projection_digest)
                SELECT $1,allocation_id,receipt_id,model_projection_digest FROM cloud_workspace_input_reads
                WHERE allocation_id=$2 AND model_projection_digest IS NOT NULL")
                .bind(&lease.run_id).bind(&lease.allocation_id).execute(&mut *tx).await.map_err(db_error)?;
            if marked.rows_affected()!=1{return Err(changed());}
        }
        leases::event(&mut tx,&lease.run_id,"workspace_materialized").await?;id
    };
    leases::confirm_live(&mut tx, &lease.workspace_id).await?;
    let (v,required):(i64,bool)=sqlx::query_as("SELECT r.version,coalesce(p.execution_protocol=1,false) FROM cloud_runs r LEFT JOIN cloud_model_plans p USING(run_id) WHERE r.run_id=$1")
        .bind(&lease.run_id).fetch_one(&mut *tx).await.map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    Ok(host::WorkspaceReadyReceipt {
        run_version: u64::try_from(v).map_err(|_| changed())?,
        requires_run_authority: required,
        receipt_id: receipt,
        workspace_id: lease.workspace_id,
        allocation_id: lease.allocation_id,
        sandbox_id: r.sandbox_id,
    })
}
