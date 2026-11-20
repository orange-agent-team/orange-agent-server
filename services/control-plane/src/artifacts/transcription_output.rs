use super::*;
use orange_protocol::{grpc::worker, pricing::AudioMeasurement, transcript::TranscriptionResult};

pub(super) struct TranscriptSource {
    pub grant_id: String,
    pub observation_id: String,
    pub projection: String,
    pub profile: String,
    pub measurement: AudioMeasurement,
    pub language: Option<String>,
    pub compute: String,
}
impl ArtifactStore {
    #[cfg(feature = "transcription-input-tests")]
    pub async fn pause_verified_transcript_publication(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (arrived, reader) = tokio::sync::oneshot::channel();
        let (go, resume) = tokio::sync::oneshot::channel();
        *self.inner.transcript_publish_gate.lock().await = Some((arrived, resume));
        (reader, go)
    }

    pub(super) async fn transcript_source(
        &self,
        tx: &mut Tx<'_>,
        session: &crate::worker::DeviceSession,
        assignment: &worker::Assignment,
        owner: &str,
        user: &str,
        historical: bool,
    ) -> Result<(worker::InputArtifact, TranscriptSource), ArtifactError> {
        let reference = assignment
            .transcription_input
            .as_ref()
            .ok_or(ArtifactError::Forbidden)?;
        let lease = assignment.lease.as_ref().ok_or(ArtifactError::Forbidden)?;
        if session.transcription_output_version != 1 {
            return Err(ArtifactError::Forbidden);
        }
        if !historical && audio_grants::job_binding(tx, &lease.job_id, session).await? != *reference
        {
            return Err(ArtifactError::Forbidden);
        }
        sqlx::query("SELECT grant_id FROM transcription_input_grants WHERE owner_id=$1 AND grant_id=$2 FOR SHARE")
            .bind(owner).bind(&reference.grant_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let grant = audio_grants::lookup(tx, owner, &reference.grant_id, None)
            .await?
            .ok_or(ArtifactError::Forbidden)?;
        if grant.try_get::<String, _>("author_id")? != user
            || grant.try_get::<String, _>("observation_id")? != reference.observation_id
            || grant.try_get::<String, _>("projection_digest")?
                != assignment.request_projection_digest
            || grant.try_get::<String, _>("grant_state")? == "revoked"
        {
            return Err(ArtifactError::Forbidden);
        }
        let compute:String=sqlx::query_scalar("SELECT compute_type FROM worker_transcription_profile_observations WHERE observation_id=$1
            AND node_id=$2 AND execution_profile_id=$3 AND execution_profile_digest=$4 AND model_sha256=$5")
            .bind(&reference.observation_id).bind(&session.node_id).bind(&assignment.execution_profile_id)
            .bind(&assignment.execution_profile_digest).bind(&assignment.model_sha256).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let measured: AudioMeasurement = serde_json::from_value(grant.try_get("measurement")?)
            .map_err(|_| ArtifactError::Integrity)?;
        let input = if historical {
            inputs::source_read_in(tx, &lease.job_id, user).await?
        } else {
            let (source, _) = self
                .authorized_audio_source(tx, user, owner, &grant)
                .await?;
            worker::InputArtifact {
                artifact_id: source.artifact_id,
                version_id: source.version_id,
                sha256: source.sha256,
                size_bytes: source
                    .size_bytes
                    .parse()
                    .map_err(|_| ArtifactError::Integrity)?,
                media_type: source.media_type,
            }
        };
        if input.artifact_id != measured.artifact_id
            || input.version_id != measured.version_id
            || input.sha256 != measured.audio_sha256
        {
            return Err(ArtifactError::Integrity);
        }
        Ok((
            input,
            TranscriptSource {
                grant_id: reference.grant_id.clone(),
                observation_id: reference.observation_id.clone(),
                projection: assignment.request_projection_digest.clone(),
                profile: assignment.execution_profile_digest.clone(),
                measurement: measured,
                language: grant.try_get("language")?,
                compute,
            },
        ))
    }
}
impl TranscriptSource {
    pub(super) fn validate(&self, bytes: &[u8], digest: &str) -> Result<(), ArtifactError> {
        let result: TranscriptionResult =
            serde_json::from_slice(bytes).map_err(|_| ArtifactError::Invalid)?;
        if !result.matches(
            &self.measurement,
            &self.projection,
            &self.profile,
            &self.compute,
            self.language.as_deref(),
        ) {
            return Err(ArtifactError::Invalid);
        }
        if request_digest("worker.transcription.result.v1", &result)
            .map_err(|_| ArtifactError::Invalid)?
            != digest
        {
            return Err(ArtifactError::Integrity);
        }
        Ok(())
    }
    pub(super) async fn record(
        &self,
        tx: &mut Tx<'_>,
        job: &str,
        owner: &str,
        input: &worker::InputArtifact,
        response: &str,
    ) -> Result<(), ArtifactError> {
        sqlx::query("INSERT INTO transcription_response_facts(job_id,owner_id,grant_id,observation_id,source_artifact_id,source_version_id,
             source_sha256,source_bytes,duration_ms,projection_digest,response_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(job).bind(owner).bind(&self.grant_id).bind(&self.observation_id).bind(&input.artifact_id).bind(&input.version_id)
            .bind(&input.sha256).bind(i64::try_from(input.size_bytes).map_err(|_|ArtifactError::Integrity)?)
            .bind(self.measurement.duration_ms.parse::<i64>().map_err(|_|ArtifactError::Integrity)?).bind(&self.projection).bind(response)
            .execute(&mut **tx).await?;
        Ok(())
    }
}
