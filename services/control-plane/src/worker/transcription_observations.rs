use super::{DeviceSession, SessionError, WorkerSessionStore, sessions};
use orange_protocol::grpc::worker::{TranscriptionObservation, TranscriptionObservationReceipt};
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub(super) enum ObservationError {
    #[error("invalid observation")]
    Invalid,
    #[error("observation conflicts with existing immutable receipt")]
    Conflict,
    #[error("profile is not approved for the observed model")]
    Unreviewed,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl WorkerSessionStore {
    pub(super) async fn record_transcription_observation(
        &self,
        session: &DeviceSession,
        report: &TranscriptionObservation,
    ) -> Result<TranscriptionObservationReceipt, ObservationError> {
        let digest = orange_protocol::execution::transcription_observation_digest(report)
            .ok_or(ObservationError::Invalid)?;
        if report.session_id != session.session_id || session.execution_control_version != 2 {
            return Err(SessionError::Unauthorized.into());
        }
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, session).await?;
        let state: String =
            sqlx::query_scalar("SELECT state FROM nodes WHERE node_id=$1 FOR SHARE")
                .bind(&session.node_id)
                .fetch_one(&mut *tx)
                .await?;
        if state != "online" {
            return Err(SessionError::Stale.into());
        }
        let approved: Option<String> = sqlx::query_scalar(
            "SELECT execution_profile_id FROM reviewed_execution_profiles
             WHERE execution_profile_id=$1 AND enabled AND execution_profile_digest=$2
               AND model_sha256=$3 AND model_version_id=$4 FOR SHARE",
        )
        .bind(&report.execution_profile_id)
        .bind(&report.execution_profile_digest)
        .bind(&report.model_sha256)
        .bind(&report.model_version)
        .fetch_optional(&mut *tx)
        .await?;
        if approved.is_none() {
            return Err(ObservationError::Unreviewed);
        }
        sqlx::query("INSERT INTO worker_transcription_profile_observations
            (observation_id,node_id,session_id,connection_epoch,request_id,execution_profile_id,execution_profile_digest,observation_digest,model_version,model_sha256,sample_sha256,transcript_sha256,duration_ms,normalized_samples,compute_type)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) ON CONFLICT DO NOTHING")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&session.node_id).bind(&session.session_id)
            .bind(session.connection_epoch).bind(&report.request_id).bind(&report.execution_profile_id)
            .bind(&report.execution_profile_digest).bind(&digest).bind(&report.model_version).bind(&report.model_sha256).bind(&report.sample_sha256).bind(&report.transcript_sha256).bind(report.duration_ms as i64).bind(report.normalized_samples as i64).bind(&report.compute_type).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT observation_id,observation_digest FROM worker_transcription_profile_observations
            WHERE node_id=$1 AND session_id=$2 AND connection_epoch=$3 AND request_id=$4")
            .bind(&session.node_id).bind(&session.session_id).bind(session.connection_epoch).bind(&report.request_id)
            .fetch_optional(&mut *tx).await?.ok_or(ObservationError::Conflict)?;
        if row.try_get::<String, _>("observation_digest")? != digest {
            return Err(ObservationError::Conflict);
        }
        let receipt = TranscriptionObservationReceipt {
            observation_version: 1,
            session_id: session.session_id.clone(),
            connection_epoch: session.connection_epoch as u64,
            request_id: report.request_id.clone(),
            observation_id: row.try_get("observation_id")?,
            execution_profile_id: report.execution_profile_id.clone(),
            execution_profile_digest: report.execution_profile_digest.clone(),
            observation_digest: digest,
        };
        let descriptor: serde_json::Value = serde_json::from_slice(&report.profile_descriptor_json)
            .map_err(|_| ObservationError::Invalid)?;
        if let Some(sha) = descriptor["runtime"]["python_sha256"]
            .as_str()
            .filter(|s| orange_protocol::valid_digest(s))
        {
            sqlx::query("INSERT INTO transcription_process_identities(observation_id,python_sha256) VALUES($1,$2) ON CONFLICT DO NOTHING")
                .bind(&receipt.observation_id).bind(sha).execute(&mut *tx).await?;
        }
        sessions::finish(tx, session).await?;
        Ok(receipt)
    }
}
