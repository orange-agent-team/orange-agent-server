use super::*;
use orange_model_runtime::transcription::Qualification;
use orange_protocol::grpc::worker::{TranscriptionObservation, TranscriptionObservationReceipt};

#[derive(Debug, thiserror::Error)]
pub enum ObservationFailure {
    #[error("the platform has not approved this profile")]
    NotApproved,
    #[error("the gateway does not implement transcription observations")]
    Unsupported,
    #[error(transparent)]
    Control(#[from] ControlError),
}

impl ControlConnection {
    /// Send only fixed public probe facts. Retrying in the same epoch uses the same request key.
    pub async fn report_transcription_profile(
        &mut self,
        profile_id: &str,
        fact: &Qualification,
    ) -> Result<TranscriptionObservationReceipt, ObservationFailure> {
        if self.execution_control_version != 2 || !self.is_current_locally() {
            return Err(ControlError::Stale.into());
        }
        let key = orange_protocol::request_digest(
            "worker.transcription-observation-key.v1",
            &serde_json::json!({
                "session_id":self.identity.session_id,"profile_id":profile_id
            }),
        )
        .map_err(|_| ControlError::Invalid)?;
        let request = TranscriptionObservation {
            observation_version: 1,
            session_id: self.identity.session_id.clone(),
            request_id: key.clone(),
            execution_profile_id: profile_id.to_owned(),
            execution_profile_digest: fact.execution_profile_digest.clone(),
            model_version: fact.profile.model_version.to_owned(),
            model_sha256: fact.profile.model_sha256.clone(),
            sample_sha256: fact.sample_sha256.clone(),
            duration_ms: fact.sample_duration_ms,
            normalized_samples: fact.normalized_samples,
            transcript_sha256: fact.transcript_sha256.clone(),
            compute_type: fact.profile.compute_type.clone(),
            profile_descriptor_json: serde_json::to_vec(&fact.profile)
                .map_err(|_| ControlError::Invalid)?,
        };
        let expected = orange_protocol::execution::transcription_observation_digest(&request)
            .ok_or(ControlError::Invalid)?;
        let end = self.stop_at.min(Instant::now() + Duration::from_secs(5));
        let receipt = tokio::time::timeout_at(
            tokio::time::Instant::from_std(end),
            self.input_client.report_transcription_observation(request),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(|e| match e.code() {
            tonic::Code::FailedPrecondition
                if e.message() == "TRANSCRIPTION_PROFILE_NOT_APPROVED" =>
            {
                ObservationFailure::NotApproved
            }
            tonic::Code::Unimplemented => ObservationFailure::Unsupported,
            _ => ObservationFailure::Control(status(e)),
        })?
        .into_inner();
        if !self.is_current_locally()
            || receipt.session_id != self.identity.session_id
            || receipt.connection_epoch != self.identity.connection_epoch
        {
            return Err(ControlError::Stale.into());
        }
        if receipt.observation_version != 1
            || receipt.request_id != key
            || receipt.execution_profile_id != profile_id
            || receipt.execution_profile_digest != fact.execution_profile_digest
            || receipt.observation_digest != expected
            || uuid::Uuid::parse_str(&receipt.observation_id).is_err()
        {
            return Err(ControlError::Protocol.into());
        }
        Ok(receipt)
    }
}
