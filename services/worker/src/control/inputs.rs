use super::execution::StartedExecution;
use super::*;
use crate::execution_journal::ExecutionJournal;
impl ControlConnection {
    pub async fn connect_execution_with_inputs(
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
                ..ControlVersions::default()
            },
        )
        .await
    }
    /// Return a model request only after full bytes/hash/projection validation.
    /// The caller must keep supervising the Broker and control connection; this
    /// method neither launches an engine nor renews any execution authority.
    pub async fn fetch_model_input(
        &mut self,
        journal: &mut ExecutionJournal,
        started: &StartedExecution,
        command: &str,
    ) -> Result<orange_protocol::model::GenerateRequest, ControlError> {
        if self.input_artifacts_version != 1 || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if record.state != "running"
            || record.grant.as_ref() != Some(started.grant())
            || record.assignment.session_id != self.identity.session_id
            || record.assignment.input_artifacts.len() != 1
        {
            return Err(ControlError::Stale);
        }
        let descriptor = record.assignment.input_artifacts[0].clone();
        if descriptor.size_bytes == 0
            || descriptor.size_bytes > 256 * 1024
            || descriptor.media_type != "application/json"
        {
            return Err(ControlError::Protocol);
        }
        let deadline = self.stop_at.min(started.stop_at());
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = false;
        let mut bytes = Vec::with_capacity(descriptor.size_bytes as usize);
        while (bytes.len() as u64) < descriptor.size_bytes {
            let offset = bytes.len() as u64;
            let request = worker::InputReadRequest {
                session_id: self.identity.session_id.clone(),
                lease: record.assignment.lease.clone(),
                artifact_id: descriptor.artifact_id.clone(),
                version_id: descriptor.version_id.clone(),
                offset,
                max_bytes: 32768,
            };
            let reply = tokio::time::timeout_at(
                tokio::time::Instant::from_std(
                    deadline.min(Instant::now() + Duration::from_secs(5)),
                ),
                self.input_client.read_input(request),
            )
            .await
            .map_err(|_| ControlError::Unavailable)?
            .map_err(status)?
            .into_inner();
            if reply.artifact.as_ref() != Some(&descriptor)
                || reply.offset != offset
                || reply.data.is_empty()
                || reply.data.len() > 32768
                || offset + reply.data.len() as u64 > descriptor.size_bytes
                || reply.eof != (offset + reply.data.len() as u64 == descriptor.size_bytes)
            {
                return Err(ControlError::Protocol);
            }
            bytes.extend(reply.data);
        }
        // Use the shared canonical digest implementation; no model invocation is
        // permitted from a partial or changed object response.
        use sha2::{Digest, Sha256};
        if hex::encode(Sha256::digest(&bytes)) != descriptor.sha256 {
            return Err(ControlError::Protocol);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ControlError::Protocol)?;
        let value = orange_protocol::canonical::parse_tool_arguments(text)
            .map_err(|_| ControlError::Protocol)?;
        let request: orange_protocol::model::GenerateRequest =
            serde_json::from_value(value).map_err(|_| ControlError::Protocol)?;
        request.validate().map_err(|_| ControlError::Protocol)?;
        if request.model_version != record.assignment.model_version_id
            || orange_protocol::request_digest("model-request-v1", &request)
                .map_err(|_| ControlError::Protocol)?
                != record.assignment.request_projection_digest
            || Instant::now() >= deadline
        {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(request)
    }
}

impl ControlConnection {
    /// Current cloud execution/data authority only; it does not renew a lease or
    /// certify local resource ownership. The execution owner must supervise that.
    pub(crate) async fn check_cpu_authority(
        &mut self,
        record: &crate::execution_journal::ExecutionRecord,
    ) -> Result<(), ControlError> {
        if self.input_artifacts_version != 1 || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        if record.assignment.transcription_input.is_some() {
            return self.check_transcription_authority(record).await;
        }
        if record.state != "running"
            || record.assignment.session_id != self.identity.session_id
            || record.assignment.input_artifacts.len() != 1
        {
            return Err(ControlError::Stale);
        }
        let input = &record.assignment.input_artifacts[0];
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(5));
        self.active = false;
        let reply = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.input_client.read_input(worker::InputReadRequest {
                session_id: self.identity.session_id.clone(),
                lease: record.assignment.lease.clone(),
                artifact_id: input.artifact_id.clone(),
                version_id: input.version_id.clone(),
                offset: 0,
                max_bytes: 1,
            }),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        if reply.artifact.as_ref() != Some(input)
            || reply.offset != 0
            || reply.data.len() != 1
            || Instant::now() >= deadline
        {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(())
    }
}
