//! Input object storage. Only available immutable versions can be read; Worker
//! delivery and output publication require separate Job/lease authorization.
pub(crate) mod authority;
mod cloud_followup;
mod cloud_turn;
mod connector_proposal;
mod delivery;
pub mod http;
pub(crate) mod inputs;
mod maintenance;
mod outputs;
mod pricing;
mod settlement;
mod storage;
mod transfer;
pub use delivery::ArtifactReader;
use orange_protocol::{
    artifacts::{ArtifactView, CreateUpload},
    request_digest, valid_id,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};
use tokio_stream::{Stream, StreamExt};
type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact request")]
    Invalid,
    #[error("artifact authorization unavailable or revoked")]
    Forbidden,
    #[error("artifact not found")]
    NotFound,
    #[error("artifact ID conflicts with original content")]
    Conflict,
    #[error("artifact is not available")]
    NotReady,
    #[error("artifact was deleted")]
    Deleted,
    #[error("artifact transfer is already in progress")]
    Busy,
    #[error("artifact storage quota exceeded")]
    Capacity,
    #[error("quote exceeds the available root budget")]
    Budget,
    #[error("approved pricing or trusted metering is unavailable")]
    PricingUnavailable,
    #[error("artifact content or version failed integrity validation")]
    Integrity,
    #[error("artifact deadline expired")]
    Expired,
    #[error("private artifact storage policy required")]
    StoragePolicy,
    #[error("artifact input stream failed")]
    Input,
    #[error("artifact worker unavailable")]
    Unavailable,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] opendal::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
/// Minted only by the real UserIdentity resolver inside this crate. No request
/// JSON, actor ID header or public constructor can establish this authority.
#[derive(Clone)]
pub struct ArtifactActor {
    pub(crate) user: String,
    pub(crate) session: String,
    pub(crate) write: bool,
}
impl ArtifactActor {
    pub(crate) fn from_verified_session(
        view: &orange_protocol::user::UserSessionView,
        write: bool,
    ) -> Self {
        Self {
            user: view.user_id.clone(),
            session: view.session_id.clone(),
            write,
        }
    }
}
#[derive(Clone)]
pub struct ArtifactStore {
    inner: Arc<Inner>,
}
struct Inner {
    pool: PgPool,
    objects: storage::LocalObjects,
    slots: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    closed: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
}
/// A current authorization revision, not a reusable delivery capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadStamp {
    organization: Option<i64>,
    acl: Option<i64>,
}
#[derive(Clone)]
struct Transfer {
    id: String,
    stage: String,
    published: String,
    view: ArtifactView,
    replay: bool,
    stamp: ReadStamp,
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn view(row: &sqlx::postgres::PgRow) -> Result<ArtifactView, ArtifactError> {
    Ok(ArtifactView {
        upload_id: row.try_get("upload_id")?,
        artifact_id: row.try_get("artifact_id")?,
        version_id: row.try_get("version_id")?,
        owner_id: row.try_get("owner_id")?,
        sha256: row.try_get("sha256")?,
        size_bytes: row.try_get::<i64, _>("size_bytes")?.to_string(),
        media_type: row.try_get("media_type")?,
        state: row.try_get("state")?,
    })
}
impl ArtifactStore {
    pub async fn open_local(
        pool: PgPool,
        root: &Path,
        byte_limit: u64,
        record_limit: u32,
    ) -> Result<Self, ArtifactError> {
        Self::open_local_with_meter(pool, root, byte_limit, record_limit, None).await
    }
    pub async fn open_local_with_meter(
        pool: PgPool,
        root: &Path,
        byte_limit: u64,
        record_limit: u32,
        meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
    ) -> Result<Self, ArtifactError> {
        if byte_limit == 0 || byte_limit > i64::MAX as u64 || record_limit == 0 {
            return Err(ArtifactError::Invalid);
        }
        let objects = storage::LocalObjects::open(root)?;
        let row=sqlx::query("INSERT INTO artifact_capacity(root_id,byte_limit,record_limit) VALUES($1,$2,$3) ON CONFLICT(root_id) DO UPDATE SET root_id=excluded.root_id RETURNING byte_limit,record_limit").bind(&objects.root_id).bind(byte_limit as i64).bind(i64::from(record_limit)).fetch_one(&pool).await?;
        if row.try_get::<i64, _>("byte_limit")? != byte_limit as i64
            || row.try_get::<i64, _>("record_limit")? != i64::from(record_limit)
        {
            return Err(ArtifactError::Conflict);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                pool,
                objects,
                slots: Arc::new(Semaphore::new(4)),
                pending: Arc::new(Semaphore::new(8)),
                closed: AtomicBool::new(false),
                tasks: Mutex::new(Vec::new()),
                meter,
            }),
        })
    }
    /// Graceful API shutdown waits for owned I/O; abandoning a request does not
    /// release an I/O slot while a filesystem writer is still running.
    pub async fn drain(&self) -> Result<(), ArtifactError> {
        let mut guard = self.inner.tasks.lock().await;
        self.inner.closed.store(true, Ordering::Release);
        let tasks = std::mem::take(&mut *guard);
        drop(guard);
        for task in tasks {
            task.await.map_err(|_| ArtifactError::Unavailable)?;
        }
        Ok(())
    }
    pub(super) async fn start_owned(
        &self,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), ArtifactError> {
        let mut tasks = self.inner.tasks.lock().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ArtifactError::Unavailable);
        }
        let mut i = 0;
        while i < tasks.len() {
            if tasks[i].is_finished() {
                tasks
                    .swap_remove(i)
                    .await
                    .map_err(|_| ArtifactError::Unavailable)?;
            } else {
                i += 1;
            }
        }
        tasks.push(tokio::spawn(future));
        Ok(())
    }
    pub async fn begin_upload(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        req: &CreateUpload,
    ) -> Result<ArtifactView, ArtifactError> {
        let size = req.size().ok_or(ArtifactError::Invalid)?;
        let digest =
            request_digest("artifact.upload.v1", req).map_err(|_| ArtifactError::Invalid)?;
        let mut tx = self.inner.pool.begin().await?;
        let role = authority::scope(&mut tx, actor, owner, true).await?;
        let capacity = sqlx::query("SELECT * FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
            .bind(&self.inner.objects.root_id)
            .fetch_one(&mut *tx)
            .await?;
        let prior=sqlx::query("SELECT a.*,u.upload_id,u.request_digest FROM artifact_uploads u JOIN artifacts a USING(artifact_id) WHERE u.owner_id=$1 AND u.upload_id=$2 FOR UPDATE OF a").bind(owner).bind(&req.upload_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = prior {
            self.check_root_row(&row)?;
            authority::resource(&mut tx, actor, owner, &row, true, true).await?;
            if row.try_get::<String, _>("request_digest")? != digest {
                return Err(ArtifactError::Conflict);
            }
            let result = view(&row)?;
            authority::finish(tx, actor).await?;
            return Ok(result);
        }
        if capacity.try_get::<i64, _>("records")? >= capacity.try_get::<i64, _>("record_limit")? {
            return Err(ArtifactError::Capacity);
        }
        let artifact = id();
        let version = id();
        if role.is_some() {
            sqlx::query("INSERT INTO team_resources(resource_id,organization_id,creator_id,kind) VALUES($1,$2,$3,'artifact')").bind(&artifact).bind(owner).bind(&actor.user).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO resource_acl(resource_id,user_id,access) VALUES($1,$2,'write')",
            )
            .bind(&artifact)
            .bind(&actor.user)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO artifacts(artifact_id,version_id,root_id,owner_id,creator_id,resource_id,sha256,size_bytes,media_type) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(&artifact).bind(&version).bind(&self.inner.objects.root_id).bind(owner).bind(&actor.user).bind(role.map(|_|artifact.clone())).bind(&req.sha256).bind(size as i64).bind(&req.media_type).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO artifact_uploads(owner_id,upload_id,artifact_id,request_digest) VALUES($1,$2,$3,$4)").bind(owner).bind(&req.upload_id).bind(&artifact).bind(&digest).execute(&mut *tx).await?;
        sqlx::query("UPDATE artifact_capacity SET records=records+1 WHERE root_id=$1")
            .bind(&self.inner.objects.root_id)
            .execute(&mut *tx)
            .await?;
        crate::events::append(
            &mut tx,
            owner,
            &artifact,
            "artifact.upload_created",
            &digest,
        )
        .await?;
        authority::finish(tx, actor).await?;
        Ok(ArtifactView {
            upload_id: req.upload_id.clone(),
            artifact_id: artifact,
            version_id: version,
            owner_id: owner.into(),
            sha256: req.sha256.clone(),
            size_bytes: size.to_string(),
            media_type: req.media_type.clone(),
            state: "staging".into(),
        })
    }
    fn check_root_row(&self, row: &sqlx::postgres::PgRow) -> Result<(), ArtifactError> {
        if row.try_get::<String, _>("root_id")? != self.inner.objects.root_id {
            return Err(ArtifactError::StoragePolicy);
        }
        Ok(())
    }
    async fn row_in(
        &self,
        tx: &mut Tx<'_>,
        owner: &str,
        upload: &str,
    ) -> Result<sqlx::postgres::PgRow, ArtifactError> {
        if !valid_id(upload) {
            return Err(ArtifactError::Invalid);
        }
        let row=sqlx::query("SELECT a.*,u.upload_id,a.created_at>clock_timestamp()-interval '1 hour' AS upload_live FROM artifact_uploads u JOIN artifacts a USING(artifact_id) WHERE u.owner_id=$1 AND u.upload_id=$2 FOR UPDATE OF a").bind(owner).bind(upload).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotFound)?;
        self.check_root_row(&row)?;
        Ok(row)
    }
    pub async fn status(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
    ) -> Result<ArtifactView, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, true).await?;
        let result = view(&row)?;
        authority::finish(tx, actor).await?;
        Ok(result)
    }
    pub async fn delete(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
    ) -> Result<ArtifactView, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        sqlx::query("SELECT root_id FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
            .bind(&self.inner.objects.root_id)
            .fetch_one(&mut *tx)
            .await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, true, true).await?;
        let mut result = view(&row)?;
        if result.state != "deleted" {
            sqlx::query("UPDATE artifact_capacity SET records=records-1 WHERE root_id=$1")
                .bind(&self.inner.objects.root_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE artifacts SET state='deleted',deleted_at=clock_timestamp() WHERE artifact_id=$1").bind(&result.artifact_id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO artifact_tombstones(artifact_id,actor_id) VALUES($1,$2)")
                .bind(&result.artifact_id)
                .bind(&actor.user)
                .execute(&mut *tx)
                .await?;
            if let Some(resource) = row.try_get::<Option<String>, _>("resource_id")? {
                sqlx::query("UPDATE team_resources SET deleted=TRUE,acl_version=acl_version+1 WHERE resource_id=$1").bind(resource).execute(&mut *tx).await?;
            }
            crate::events::append(
                &mut tx,
                owner,
                &result.artifact_id,
                "artifact.deleted",
                &result.version_id,
            )
            .await?;
        }
        result.state = "deleted".into();
        authority::finish(tx, actor).await?;
        Ok(result)
    }
}
#[cfg(all(test, feature = "integration-tests"))]
mod tests;
