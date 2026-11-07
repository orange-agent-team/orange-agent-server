//! Negotiated negative decisions and immutable receipt recovery. Neither path
//! starts a model, confirms physical cleanup, settles a bill or replays a Job.
use super::*;
use crate::{
    broker::{Grant, Supervisor},
    execution_journal::{ExecutionJournal, RejectionKind, RejectionView},
};
#[derive(Debug)]
pub enum ExecutionPreparation {
    Reserved(Grant),
    Rejected(RejectionView),
}
impl ControlConnection {
    pub async fn connect_execution_with_rejections(
        config: ControlConfig,
        node_journal: &mut NodeJournal,
        journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node_journal,
            journal,
            ControlVersions {
                rejection_receipts_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    /// Only known pre-reservation refusals become durable negative decisions.
    /// I/O or unknown Broker errors are not reclassified as safe rejection.
    pub fn prepare_execution(
        &mut self,
        journal: &mut ExecutionJournal,
        broker: &mut Broker,
        supervisor: &Supervisor,
        device: &str,
        command: &str,
    ) -> Result<ExecutionPreparation, ControlError> {
        if self.rejection_receipts_version != 1 {
            return Err(ControlError::Unsupported);
        }
        if !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if record.state != "received"
            || record.assignment.session_id != self.identity.session_id
            || record.assignment.node_id != self.identity.node_id
            || record.assignment.node_policy_generation != self.observed_policy_generation
        {
            return Err(ControlError::Stale);
        }
        let kind = match broker.reserve_assignment(supervisor, device, &record.assignment) {
            Ok(grant) => {
                journal
                    .reserve(self.identity(), command, &grant)
                    .map_err(|_| ControlError::Unavailable)?;
                return Ok(ExecutionPreparation::Reserved(grant));
            }
            Err(BrokerError::ProfileRejected) => RejectionKind::Protocol,
            Err(
                BrokerError::Occupied
                | BrokerError::RemoteDisabled
                | BrokerError::PlatformBlocked
                | BrokerError::RecoveryRequired,
            ) => RejectionKind::Resource,
            Err(_) => return Err(ControlError::Unavailable),
        };
        let decision = journal
            .reject(self.identity(), command, kind)
            .map_err(|_| ControlError::Unavailable)?;
        Ok(ExecutionPreparation::Rejected(decision))
    }
    /// Sends only the already committed local decision on its original session.
    pub async fn send_rejection(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
    ) -> Result<RejectionView, ControlError> {
        self.rejection_exchange(journal, command, false).await
    }
    /// Historical metadata query. It can use a new authenticated session but
    /// cannot turn unknown/offered/started cloud state into a negative decision.
    pub async fn query_rejection(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
    ) -> Result<RejectionView, ControlError> {
        self.rejection_exchange(journal, command, true).await
    }
    async fn rejection_exchange(
        &mut self,
        journal: &mut ExecutionJournal,
        command: &str,
        historical: bool,
    ) -> Result<RejectionView, ControlError> {
        if self.rejection_receipts_version != 1 {
            return Err(ControlError::Unsupported);
        }
        if !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let ack = journal
            .rejected_ack(self.identity(), command)
            .map_err(|_| ControlError::Stale)?;
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if !historical
            && (record.assignment.session_id != self.identity.session_id
                || ack
                    .lease
                    .as_ref()
                    .is_none_or(|l| l.connection_epoch != self.identity.connection_epoch))
        {
            return Err(ControlError::Stale);
        }
        if self
            .offered
            .as_ref()
            .is_some_and(|a| a.command_id == command)
        {
            self.offered = None;
        }
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(6));
        self.active = false;
        let operation = async {
            let body = if historical {
                worker_frame::Body::RejectionQuery(worker::RejectionQuery {
                    rejected: Some(ack),
                })
            } else {
                worker_frame::Body::Acknowledge(ack)
            };
            self.sender
                .send(worker::WorkerFrame { body: Some(body) })
                .await
                .map_err(|_| ControlError::Unavailable)?;
            for _ in 0..16 {
                match self
                    .stream
                    .message()
                    .await
                    .map_err(status)?
                    .ok_or(ControlError::Unavailable)?
                    .body
                {
                    Some(server_frame::Body::Assignment(a)) if a == record.assignment => continue,
                    Some(server_frame::Body::Assignment(a)) => self.cache_assignment(&a)?,
                    Some(server_frame::Body::RejectionReceipt(receipt)) => return Ok(receipt),
                    _ => return Err(ControlError::Protocol),
                }
            }
            Err(ControlError::Protocol)
        };
        let receipt = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .map_err(|_| ControlError::Unavailable)??;
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        let result = journal
            .confirm_rejection(self.identity(), command, &receipt)
            .map_err(|_| ControlError::Unavailable)?;
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = true;
        Ok(result)
    }
}
