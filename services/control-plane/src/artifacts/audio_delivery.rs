//! Every audio range is separately authorized and verified against hashes frozen at consent.
use super::*;
use crate::worker::{DeviceSession, WorkerSessionStore};
use orange_protocol::{grpc::worker, pricing::AUDIO_INPUT_CHUNK_BYTES};

#[derive(PartialEq, Eq)]
struct AuthorizedChunk {
    measurement: Vec<u8>,
    language: String,
    key: String,
    artifact: String,
    version: String,
    sha: String,
    chunk_sha: String,
    size: u64,
}

#[cfg(feature = "transcription-input-tests")]
pub(super) struct AudioPause {
    offset: u64,
    arrived: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(feature = "transcription-input-tests")]
impl ArtifactReader {
    pub async fn pause_after_verified_audio_chunk(
        &self,
        offset: u64,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (arrived, seen) = tokio::sync::oneshot::channel();
        let (go, resume) = tokio::sync::oneshot::channel();
        *self.audio_pause.lock().await = Some(AudioPause {
            offset,
            arrived,
            resume,
        });
        (seen, go)
    }
}
impl ArtifactReader {
    async fn audio_range_authority(
        &self,
        tx: &mut Tx<'_>,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: &worker::TranscriptionInputRead,
    ) -> Result<AuthorizedChunk, ArtifactError> {
        let lease = r.lease.as_ref().ok_or(ArtifactError::Invalid)?;
        let assignment = self
            .started_assignment(tx, sessions, s, &r.session_id, lease)
            .await?;
        if assignment.request_projection_digest != r.projection_digest
            || !assignment.input_artifacts.is_empty()
            || !assignment.input_artifact_ids.is_empty()
        {
            return Err(ArtifactError::Forbidden);
        }
        let job=sqlx::query("SELECT owner_id,root_budget_id,invocation_id,submitting_user_id FROM jobs WHERE job_id=$1 AND purpose='transcription' AND execution_state='running' AND settlement_state='pending'")
            .bind(&lease.job_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let owner: String = job.try_get("owner_id")?;
        let user = job
            .try_get::<Option<String>, _>("submitting_user_id")?
            .ok_or(ArtifactError::Forbidden)?;
        // Take the consent row lock before checking the separate append-only revocation.
        sqlx::query("SELECT grant_id FROM transcription_input_grants WHERE owner_id=$1 AND grant_id=$2 FOR SHARE")
            .bind(&owner).bind(&r.grant_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let grant = super::audio_grants::lookup(
            tx,
            &owner,
            &r.grant_id,
            Some((r.offset / AUDIO_INPUT_CHUNK_BYTES as u64) as i32 + 1),
        )
        .await?
        .ok_or(ArtifactError::Forbidden)?;
        if grant.try_get::<String, _>("grant_state")? != "active"
            || grant.try_get::<String, _>("author_id")? != user
            || grant.try_get::<String, _>("invocation_id")?
                != job.try_get::<String, _>("invocation_id")?
            || grant.try_get::<String, _>("root_budget_id")?
                != job.try_get::<String, _>("root_budget_id")?
            || grant.try_get::<String, _>("projection_digest")? != r.projection_digest
        {
            return Err(ArtifactError::Forbidden);
        }
        let target =
            super::audio_grants::observed_target(tx, grant.try_get("observation_id")?).await?;
        if target.try_get::<String, _>("region_id")? != grant.try_get::<String, _>("region_id")?
            || target.try_get::<String, _>("session_id")? != s.session_id
            || target.try_get::<String, _>("node_id")? != s.node_id
            || target.try_get::<i64, _>("connection_epoch")? != s.connection_epoch
            || target.try_get::<String, _>("execution_profile_id")?
                != assignment.execution_profile_id
            || target.try_get::<String, _>("execution_profile_digest")?
                != assignment.execution_profile_digest
        {
            return Err(ArtifactError::Forbidden);
        }
        let (source, transfer) = self
            .store
            .authorized_audio_source(tx, &user, &owner, &grant)
            .await?;
        let size = source
            .size_bytes
            .parse::<u64>()
            .map_err(|_| ArtifactError::Integrity)?;
        if r.offset >= size {
            return Err(ArtifactError::Invalid);
        }
        let chunk_sha: String = grant
            .try_get::<Option<String>, _>("chunk_sha")?
            .ok_or(ArtifactError::Integrity)?;
        let key:String=sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE transfer_id=$1 AND artifact_id=$2 AND state='attached' AND NOT published_deleted")
            .bind(transfer).bind(&source.artifact_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        Ok(AuthorizedChunk {
            measurement: if r.offset == 0 {
                serde_json::to_vec(&grant.try_get::<serde_json::Value, _>("measurement")?)
                    .map_err(|_| ArtifactError::Integrity)?
            } else {
                Vec::new()
            },
            language: if r.offset == 0 {
                grant
                    .try_get::<Option<String>, _>("language")?
                    .unwrap_or_default()
            } else {
                String::new()
            },
            key,
            artifact: source.artifact_id,
            version: source.version_id,
            sha: source.sha256,
            chunk_sha,
            size,
        })
    }
    pub(crate) async fn read_transcription_input(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: &worker::TranscriptionInputRead,
    ) -> Result<worker::TranscriptionInputChunk, ArtifactError> {
        if r.version != 1
            || !valid_id(&r.grant_id)
            || !orange_protocol::valid_digest(&r.projection_digest)
            || !r.offset.is_multiple_of(AUDIO_INPUT_CHUNK_BYTES as u64)
            || r.offset >= orange_protocol::artifacts::MEDIA_LIMIT
        {
            return Err(ArtifactError::Invalid);
        }
        let _permit = self
            .store
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let mut tx = self.store.inner.pool.begin().await?;
        let expected = self.audio_range_authority(&mut tx, sessions, s, r).await?;
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let data = self
            .store
            .inner
            .objects
            .verified_audio_chunk(&expected.key, expected.size, r.offset, &expected.chunk_sha)
            .await?;
        #[cfg(feature = "transcription-input-tests")]
        {
            let pause = self
                .audio_pause
                .lock()
                .await
                .take_if(|v| v.offset == r.offset);
            if let Some(pause) = pause {
                let _ = pause.arrived.send(());
                pause.resume.await.map_err(|_| ArtifactError::Unavailable)?;
            }
        }
        let mut tx = self.store.inner.pool.begin().await?;
        if self.audio_range_authority(&mut tx, sessions, s, r).await? != expected {
            return Err(ArtifactError::Forbidden);
        }
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        Ok(worker::TranscriptionInputChunk {
            version: 1,
            grant_id: r.grant_id.clone(),
            projection_digest: r.projection_digest.clone(),
            artifact_id: expected.artifact,
            version_id: expected.version,
            audio_sha256: expected.sha,
            offset: r.offset,
            size_bytes: expected.size,
            eof: r.offset + data.len() as u64 == expected.size,
            data,
            chunk_sha256: expected.chunk_sha,
            measurement_json: expected.measurement,
            language: expected.language,
        })
    }
}
