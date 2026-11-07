use super::*;
use crate::execution_journal::ExecutionJournal;
impl ControlConnection {
    pub async fn connect_managed_cpu(
        config: ControlConfig,
        node: &mut NodeJournal,
        journal: &mut ExecutionJournal,
        outputs: bool,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node,
            journal,
            ControlVersions {
                rejection_receipts_version: 1,
                input_artifacts_version: 1,
                renewal_version: 1,
                output_version: u32::from(outputs),
                cpu_cleanup_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub async fn connect_cleanup_only(
        config: ControlConfig,
        node: &mut NodeJournal,
        journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node,
            journal,
            ControlVersions {
                cpu_cleanup_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub async fn confirm_cpu_cleanup(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
    ) -> Result<worker::CpuCleanupReceipt, ControlError> {
        if self.cpu_cleanup_version != 1
            || !self.is_current_locally()
            || journal.node_id() != self.identity.node_id
        {
            return Err(ControlError::Stale);
        }
        let request = journal
            .cleanup_request(command, &self.identity.session_id)
            .map_err(|_| ControlError::Stale)?;
        let ack = request.accepted.as_ref().ok_or(ControlError::Invalid)?;
        let lease = ack.lease.as_ref().ok_or(ControlError::Invalid)?;
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(10));
        self.active = false;
        let result = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.input_client.confirm_cpu_cleanup(request.clone()),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        if result.command_id != command
            || result.attempt_id != lease.attempt_id
            || result.job_id != lease.job_id
            || result.request_digest != request.request_digest
            || !valid_id(&result.receipt_id)
            || !matches!(
                result.outcome.as_str(),
                "succeeded" | "failed" | "cancelled"
            )
            || (result.outcome == "succeeded") != !result.response_commit_id.is_empty()
        {
            return Err(ControlError::Protocol);
        }
        journal
            .confirm_cpu_cleanup(command, &result)
            .map_err(|_| ControlError::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(result)
    }
}
