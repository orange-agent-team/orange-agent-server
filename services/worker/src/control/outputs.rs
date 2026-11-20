use super::*;
use crate::execution_journal::ExecutionJournal;
use orange_protocol::execution::output_header_digest;
use sha2::{Digest, Sha256};
pub(crate) fn verify_receipt(
    h: &worker::OutputHeader,
    digest: &str,
    r: &worker::OutputReceipt,
) -> Result<(), ControlError> {
    let artifact = r.artifact.as_ref().ok_or(ControlError::Protocol)?;
    let job = &h
        .accepted
        .as_ref()
        .and_then(|a| a.lease.as_ref())
        .ok_or(ControlError::Protocol)?
        .job_id;
    if !valid_id(&r.commit_id)
        || !valid_id(&artifact.artifact_id)
        || !valid_id(&artifact.version_id)
        || r.header_digest != digest
        || r.upload_id != h.upload_id
        || &r.job_id != job
        || r.response_digest != h.response_digest
        || artifact.sha256 != h.sha256
        || artifact.size_bytes != h.size_bytes
        || artifact.media_type
            != if h.transcription_output_version == 1 {
                orange_protocol::transcript::MEDIA
            } else {
                "application/vnd.orange.model-response+json"
            }
    {
        return Err(ControlError::Protocol);
    }
    Ok(())
}
impl ControlConnection {
    pub async fn connect_output_execution(
        config: ControlConfig,
        node: &mut NodeJournal,
        journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node,
            journal,
            ControlVersions {
                rejection_receipts_version: 1,
                input_artifacts_version: 1,
                renewal_version: 1,
                output_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub async fn connect_transcription_outputs(
        config: ControlConfig,
        node: &mut NodeJournal,
        journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node,
            journal,
            ControlVersions {
                rejection_receipts_version: 1,
                input_artifacts_version: 1,
                renewal_version: 1,
                output_version: 1,
                transcription_input_version: 1,
                transcription_output_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub fn supports_output(&self) -> bool {
        self.output_version == 1
    }
    /// Consumes no new model effect. An uncertain send is query-only thereafter;
    /// this function cannot generate a second upload identity for that command.
    pub async fn commit_output(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
        candidate: &orange_protocol::model::ResponseCandidate,
    ) -> Result<worker::OutputReceipt, ControlError> {
        let bytes = serde_json::to_vec(candidate).map_err(|_| ControlError::Invalid)?;
        let digest = orange_protocol::request_digest("worker.cpu.candidate.v1", candidate)
            .map_err(|_| ControlError::Invalid)?;
        self.commit_candidate(journal, command, bytes, digest, 0)
            .await
    }
    pub async fn commit_transcription_output(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
        candidate: &orange_protocol::transcript::TranscriptionResult,
    ) -> Result<worker::OutputReceipt, ControlError> {
        if self.transcription_output_version != 1 {
            return Err(ControlError::Stale);
        }
        let bytes = serde_json::to_vec(candidate).map_err(|_| ControlError::Invalid)?;
        let digest = orange_protocol::request_digest("worker.transcription.result.v1", candidate)
            .map_err(|_| ControlError::Invalid)?;
        self.commit_candidate(journal, command, bytes, digest, 1)
            .await
    }
    async fn commit_candidate(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
        bytes: Vec<u8>,
        response_digest: String,
        version: u32,
    ) -> Result<worker::OutputReceipt, ControlError> {
        if !self.supports_output() || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        let h = worker::OutputHeader {
            transcription_output_version: version,
            session_id: self.identity.session_id.clone(),
            upload_id: uuid::Uuid::new_v4().to_string(),
            accepted: Some(record.ack().map_err(|_| ControlError::Stale)?),
            sha256: hex::encode(Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
            response_digest,
        };
        let digest = output_header_digest(&h, &record.assignment).ok_or(ControlError::Invalid)?;
        journal
            .prepare_output(self.identity(), command, &h, &digest)
            .map_err(|_| ControlError::Unavailable)?;
        let mut frames = Vec::with_capacity(bytes.len() / 32768 + 2);
        frames.push(worker::OutputFrame {
            body: Some(worker::output_frame::Body::Header(h.clone())),
        });
        for (i, chunk) in bytes.chunks(32768).enumerate() {
            frames.push(worker::OutputFrame {
                body: Some(worker::output_frame::Body::Data(worker::OutputData {
                    offset: (i * 32768) as u64,
                    data: chunk.to_vec(),
                })),
            });
        }
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(20));
        self.active = false;
        let receipt = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.input_client.commit_output(tokio_stream::iter(frames)),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        verify_receipt(&h, &digest, &receipt)?;
        journal
            .confirm_output(command, &receipt)
            .map_err(|_| ControlError::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(receipt)
    }
    /// A historical receipt never restores an execution lease or model replay.
    pub async fn query_output(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
    ) -> Result<worker::OutputReceipt, ControlError> {
        if !self.supports_output() || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let (h, digest) = journal
            .output_header(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if journal.node_id() != self.identity.node_id
            || output_header_digest(&h, &record.assignment).as_deref() != Some(digest.as_str())
        {
            return Err(ControlError::Protocol);
        }
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(5));
        self.active = false;
        let receipt = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.input_client.query_output(worker::OutputQuery {
                session_id: self.identity.session_id.clone(),
                original: Some(h.clone()),
            }),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        verify_receipt(&h, &digest, &receipt)?;
        journal
            .confirm_output(command, &receipt)
            .map_err(|_| ControlError::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(receipt)
    }
}
