use super::*;
use crate::worker::{DeviceSession, WorkerSessionStore};
use orange_protocol::grpc::worker;
use prost::Message;
#[derive(Clone)]
pub struct ArtifactReader {
    pub(super) store: ArtifactStore,
    owns_io: bool,
    #[cfg(feature = "transcription-input-tests")]
    pub(super) audio_pause: Arc<Mutex<Option<super::audio_delivery::AudioPause>>>,
}
impl ArtifactStore {
    pub fn reader(&self) -> ArtifactReader {
        ArtifactReader {
            store: self.clone(),
            owns_io: false,
            #[cfg(feature = "transcription-input-tests")]
            audio_pause: Default::default(),
        }
    }
}
impl ArtifactReader {
    /// A separate Gateway process may open the immutable data root read-only;
    /// it never obtains the writer lock or exposes user mutation methods.
    pub async fn open_local(pool: PgPool, root: &Path) -> Result<Self, ArtifactError> {
        let objects = storage::LocalObjects::open_read_only(root)?.into();
        Self::open_backend(pool, objects).await
    }
    pub async fn configured(
        pool: PgPool,
        local: Option<std::ffi::OsString>,
        remote: Option<std::ffi::OsString>,
    ) -> Result<Option<Self>, ArtifactError> {
        match (local, remote) {
            (None, None) => Ok(None),
            (Some(path), None) => Ok(Some(Self::open_local(pool, Path::new(&path)).await?)),
            (None, Some(path)) => Ok(Some(Self::open_versioned(pool, Path::new(&path)).await?)),
            _ => Err(ArtifactError::StoragePolicy),
        }
    }
    pub async fn open_versioned(pool: PgPool, config: &Path) -> Result<Self, ArtifactError> {
        let objects = versioned_store::VersionedStore::open(pool.clone(), config, false)
            .await?
            .into();
        Self::open_backend(pool, objects).await
    }
    async fn open_backend(
        pool: PgPool,
        objects: backend::ObjectBackend,
    ) -> Result<Self, ArtifactError> {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifact_capacity WHERE root_id=$1)")
                .bind(&objects.root_id)
                .fetch_one(&pool)
                .await?;
        if !known {
            return Err(ArtifactError::StoragePolicy);
        }
        Ok(Self {
            owns_io: true,
            #[cfg(feature = "transcription-input-tests")]
            audio_pause: Default::default(),
            store: ArtifactStore {
                inner: Arc::new(Inner {
                    #[cfg(feature = "transcription-input-tests")]
                    transcript_publish_gate: Default::default(),
                    pool,
                    objects,
                    slots: Arc::new(Semaphore::new(4)),
                    pending: Arc::new(Semaphore::new(8)),
                    closed: AtomicBool::new(false),
                    tasks: Mutex::new(vec![]),
                    meter: None,
                }),
            },
        })
    }
    /// A borrowed reader shares its writer's lifecycle; only an independently
    /// opened Gateway reader closes its own I/O owner here.
    pub async fn drain(&self) -> Result<(), ArtifactError> {
        if self.owns_io {
            self.store.drain().await
        } else {
            Ok(())
        }
    }
    pub(crate) async fn read_input(
        &self,
        sessions: WorkerSessionStore,
        session: DeviceSession,
        request: worker::InputReadRequest,
    ) -> Result<worker::InputReadReply, ArtifactError> {
        let pending = self
            .store
            .inner
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let reader = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.store
            .start_owned(async move {
                let _pending = pending;
                let result = reader.read_owned(&sessions, &session, &request).await;
                let _ = send.send(result);
            })
            .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }
    pub(super) async fn started_assignment(
        &self,
        tx: &mut Tx<'_>,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        session_id: &str,
        lease: &worker::Lease,
    ) -> Result<worker::Assignment, ArtifactError> {
        sessions
            .authorize_in(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        if s.input_artifacts_version != 1 || session_id != s.session_id {
            return Err(ArtifactError::Invalid);
        }
        let (node_state, generation): (String, i64) =
            sqlx::query_as("SELECT state,policy_generation FROM nodes WHERE node_id=$1")
                .bind(&s.node_id)
                .fetch_one(&mut **tx)
                .await?;
        if node_state != "online" || lease.connection_epoch != s.connection_epoch as u64 {
            return Err(ArtifactError::Forbidden);
        }
        let scoped:Option<String>=sqlx::query_scalar("SELECT job_id FROM worker_execution_attempts WHERE attempt_id=$1 AND node_id=$2 AND session_id=$3 AND connection_epoch=$4 AND lease_id=$5 AND fencing_token=$6 AND state='started' AND lease_expires_at>clock_timestamp() AND policy_generation=$7").bind(&lease.attempt_id).bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).bind(&lease.lease_id).bind(i64::try_from(lease.fencing_token).map_err(|_|ArtifactError::Invalid)?).bind(generation).fetch_optional(&mut **tx).await?;
        if scoped.as_deref() != Some(lease.job_id.as_str()) {
            return Err(ArtifactError::Forbidden);
        }
        if crate::worker::execution::job_authority(tx, &lease.job_id)
            .await
            .map_err(|_| ArtifactError::Forbidden)?
            != "running"
        {
            return Err(ArtifactError::Forbidden);
        }
        crate::worker::execution::eligible_plan(tx, &lease.job_id, &s.node_id)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let row=sqlx::query("SELECT a.assignment,p.execution_profile_id FROM worker_execution_attempts a JOIN jobs j USING(job_id) JOIN quote_execution_plans p USING(quote_id) JOIN reviewed_execution_profiles r USING(execution_profile_id) WHERE a.attempt_id=$1 AND r.enabled AND p.job_deadline>clock_timestamp() FOR SHARE OF r").bind(&lease.attempt_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let bytes: Vec<u8> = row.try_get("assignment")?;
        let assignment =
            worker::Assignment::decode(bytes.as_slice()).map_err(|_| ArtifactError::Integrity)?;
        if assignment.node_id != s.node_id
            || assignment.session_id != s.session_id
            || assignment.node_policy_generation != generation as u64
            || assignment.lease.as_ref() != Some(lease)
            || orange_protocol::execution::assignment_digest(&assignment).as_deref()
                != Some(assignment.payload_digest.as_str())
        {
            return Err(ArtifactError::Forbidden);
        }
        Ok(assignment)
    }
    async fn authority(
        &self,
        tx: &mut Tx<'_>,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: &worker::InputReadRequest,
    ) -> Result<(worker::InputArtifact, String), ArtifactError> {
        if r.max_bytes == 0 || r.max_bytes > 32768 {
            return Err(ArtifactError::Invalid);
        }
        let lease = r.lease.as_ref().ok_or(ArtifactError::Invalid)?;
        let assignment = self
            .started_assignment(tx, sessions, s, &r.session_id, lease)
            .await?;
        let (input, key) = self
            .bound_input(tx, &assignment, lease, &r.artifact_id, &r.version_id)
            .await?;
        if r.offset > input.size_bytes {
            return Err(ArtifactError::Forbidden);
        }
        Ok((input, key))
    }
    pub(super) async fn bound_input(
        &self,
        tx: &mut Tx<'_>,
        assignment: &worker::Assignment,
        lease: &worker::Lease,
        artifact_id: &str,
        version_id: &str,
    ) -> Result<(worker::InputArtifact, String), ArtifactError> {
        let input = super::inputs::check_input_in(tx, &lease.job_id)
            .await?
            .ok_or(ArtifactError::Forbidden)?;
        if assignment.input_artifacts != vec![input.clone()]
            || artifact_id != input.artifact_id
            || version_id != input.version_id
        {
            return Err(ArtifactError::Forbidden);
        }
        let row=sqlx::query("SELECT a.root_id,t.published_key FROM artifacts a JOIN artifact_transfers t ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1 AND a.state='available' AND orange_artifact_content_live(a.artifact_id) AND t.state='attached' AND NOT t.published_deleted").bind(&input.artifact_id).fetch_one(&mut **tx).await?;
        if row.try_get::<String, _>("root_id")? != self.store.inner.objects.root_id {
            return Err(ArtifactError::StoragePolicy);
        }
        Ok((input, row.try_get("published_key")?))
    }
    async fn read_owned(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: &worker::InputReadRequest,
    ) -> Result<worker::InputReadReply, ArtifactError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let _slot = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.store.inner.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ArtifactError::Expired)?
        .map_err(|_| ArtifactError::Unavailable)?;
        let mut tx = self.store.inner.pool.begin().await?;
        let (input, key) = self.authority(&mut tx, sessions, s, r).await?;
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let data = self
            .store
            .inner
            .objects
            .read(&key, input.size_bytes)
            .await?;
        let mut hash = Sha256::new();
        for part in data.clone() {
            hash.update(&part);
        }
        if hex::encode(hash.finalize()) != input.sha256 {
            return Err(ArtifactError::Integrity);
        }
        let mut tx = self.store.inner.pool.begin().await?;
        let (current, current_key) = self.authority(&mut tx, sessions, s, r).await?;
        if current != input || current_key != key {
            return Err(ArtifactError::Integrity);
        }
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        let end = (r.offset + u64::from(r.max_bytes)).min(input.size_bytes);
        let bytes = data
            .to_bytes()
            .slice(r.offset as usize..end as usize)
            .to_vec();
        Ok(worker::InputReadReply {
            artifact: Some(input),
            offset: r.offset,
            data: bytes,
            eof: end == current.size_bytes,
        })
    }
}

impl ArtifactReader {
    /// Called with a transfer reference selected by the guarded support query.
    /// Verify the whole immutable object before projecting a bounded UTF-8 range.
    pub(crate) async fn support_excerpt(
        &self,
        root: &str,
        key: &str,
        size: u64,
        digest: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(String, bool), ArtifactError> {
        if root != self.store.inner.objects.root_id
            || size > 104857600
            || limit == 0
            || limit > 65536
        {
            return Err(ArtifactError::Invalid);
        }
        let slot = self
            .store
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let bytes = self.store.inner.objects.read(key, size).await?.to_bytes();
        let expected = digest.to_owned();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            if hex::encode(Sha256::digest(&bytes)) != expected {
                return Err(ArtifactError::Integrity);
            }
            let s = std::str::from_utf8(&bytes).map_err(|_| ArtifactError::Invalid)?;
            if offset > s.len() || !s.is_char_boundary(offset) {
                return Err(ArtifactError::Invalid);
            }
            let mut end = offset.saturating_add(limit).min(s.len());
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            if end == offset && end < s.len() {
                return Err(ArtifactError::Invalid);
            }
            Ok((s[offset..end].to_owned(), end < s.len()))
        })
        .await
        .map_err(|_| ArtifactError::Integrity)?
    }
}

impl ArtifactReader {
    /// Only a server-resolved immutable version can be delivered to a cloud
    /// provisioning lease. Never accept an object key directly from a Host.
    pub(crate) async fn cloud_text_input(
        &self,
        root: &str,
        key: &str,
        size: u64,
        digest: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        if root != self.store.inner.objects.root_id || !(1..=262144).contains(&size) {
            return Err(ArtifactError::Invalid);
        }
        let _slot = self
            .store
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let bytes = self.store.inner.objects.read(key, size).await?.to_bytes();
        if hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(ArtifactError::Integrity);
        }
        std::str::from_utf8(&bytes).map_err(|_| ArtifactError::Invalid)?;
        Ok(bytes.to_vec())
    }
}
