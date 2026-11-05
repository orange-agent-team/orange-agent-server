//! Transport-authenticated assignment to local durable ACK and start barrier.
//! There is no engine launch or physical resource cleanup in this adapter.
use super::*;
use crate::{
    broker::{Grant, RemoteBinding, Supervisor, VerifiedCloudGrant},
    execution_journal::{ExecutionJournal, ExecutionRecord},
};
#[derive(Debug)]
pub struct ExecutionPoll {
    pub node_intents: Vec<ReceivedNodeIntent>,
    pub assignment: Option<ExecutionRecord>,
}
/// Consuming this value changes local Broker state once. It cannot be serialized
/// or obtained from a caller-supplied StartGrant. It borrows the live connection.
pub struct AuthorizedExecution<'a> {
    connection: &'a mut ControlConnection,
    start: worker::StartGrant,
    sent: Instant,
    deadline: Instant,
    confirm_before: Instant,
    consumed: bool,
}
/// Evidence of durable local start confirmation, not proof of an engine launch.
/// A future execution Host must own the process, heartbeat and cancellation loop.
pub struct StartedExecution {
    grant: Grant,
    deadline: Instant,
}
impl StartedExecution {
    pub fn grant(&self) -> &Grant {
        &self.grant
    }
    pub fn stop_at(&self) -> Instant {
        self.deadline
    }
}
impl Drop for AuthorizedExecution<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.connection.active = false;
        }
    }
}
impl AuthorizedExecution<'_> {
    /// Deadline for the local confirmation operation, not the model run lease.
    pub fn confirm_before(&self) -> Instant {
        self.confirm_before
    }
    pub fn confirm(
        mut self,
        journal: &mut ExecutionJournal,
        broker: &mut Broker,
        supervisor: &Supervisor,
    ) -> Result<StartedExecution, ControlError> {
        if !self.connection.is_current_locally() || Instant::now() >= self.confirm_before {
            return Err(ControlError::Stale);
        }
        let grant = journal
            .claim_start(
                self.connection.identity(),
                &self.start.command_id,
                &self.start,
            )
            .map_err(|_| ControlError::Stale)?;
        let lease = self.start.lease.as_ref().ok_or(ControlError::Protocol)?;
        let cloud = VerifiedCloudGrant::from_verified_start(
            RemoteBinding {
                node_id: self.connection.identity.node_id.clone(),
                attempt_id: lease.attempt_id.clone(),
                lease_id: lease.lease_id.clone(),
                connection_epoch: lease.connection_epoch,
                fencing_token: lease.fencing_token,
            },
            &grant,
            self.sent,
            Duration::from_millis(self.start.start_budget_ms),
        )
        .map_err(|_| ControlError::Stale)?;
        let local_stop = self.deadline.min(Instant::now() + Duration::from_secs(14));
        broker
            .confirm_start(supervisor, &grant, Some(&cloud))
            .map_err(|_| ControlError::Stale)?;
        let recorded = journal.running(&self.start.command_id);
        let expired = Instant::now() >= local_stop || Instant::now() >= self.confirm_before;
        if recorded.is_err() || expired {
            // Storage can block across the authorization deadline. Recheck AFTER
            // both durable commits before handing any permission to an engine.
            // Drain may itself fail; never claim physical release in either case.
            broker
                .begin_drain(supervisor, &grant)
                .map_err(|_| ControlError::Unavailable)?;
            return Err(if expired {
                ControlError::Stale
            } else {
                ControlError::Unavailable
            });
        }
        self.consumed = true;
        Ok(StartedExecution {
            grant,
            deadline: local_stop,
        })
    }
}
impl ControlConnection {
    pub async fn connect_execution(
        config: ControlConfig,
        node_journal: &mut NodeJournal,
        execution_journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        if config.node_id != node_journal.node_id() || config.node_id != execution_journal.node_id()
        {
            return Err(ControlError::Invalid);
        }
        let connection = Self::connect_mode(config, 2, 2).await?;
        node_journal
            .bind(connection.identity())
            .map_err(|_| ControlError::Unavailable)?;
        execution_journal
            .bind(connection.identity())
            .map_err(|_| ControlError::Unavailable)?;
        Ok(connection)
    }
    /// Heartbeat is an ordered authority check, not an execution lease renewal.
    /// Node intents must still be applied via the existing fresh-policy adapter.
    pub async fn poll_execution(
        &mut self,
        snapshot: &str,
        node_journal: &mut NodeJournal,
        execution_journal: &mut ExecutionJournal,
    ) -> Result<ExecutionPoll, ControlError> {
        if self.execution_control_version != 2 {
            return Err(ControlError::Unsupported);
        }
        let node_intents = self.heartbeat_node_control(snapshot, node_journal).await?;
        let assignment = if let Some(a) = self.offered.take() {
            if a.node_policy_generation != self.observed_policy_generation {
                None
            } else {
                Some(
                    execution_journal
                        .receive(self.identity(), &a)
                        .map_err(|_| ControlError::Unavailable)?,
                )
            }
        } else {
            None
        };
        Ok(ExecutionPoll {
            node_intents,
            assignment,
        })
    }
    /// Synchronous: no Broker borrow spans a network await. The actual local
    /// grant is committed before this Journal can produce an accepted ACK.
    pub fn reserve_execution(
        &mut self,
        journal: &mut ExecutionJournal,
        broker: &mut Broker,
        supervisor: &Supervisor,
        device: &str,
        command: &str,
    ) -> Result<Grant, ControlError> {
        if self.execution_control_version != 2 || !self.is_current_locally() {
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
        let grant = broker
            .reserve_assignment(supervisor, device, &record.assignment)
            .map_err(|_| ControlError::Unavailable)?;
        journal
            .reserve(self.identity(), command, &grant)
            .map_err(|_| ControlError::Unavailable)?;
        Ok(grant)
    }
    pub async fn authorize_execution<'a>(
        &'a mut self,
        journal: &mut ExecutionJournal,
        node_journal: &mut NodeJournal,
        snapshot: &str,
        command: &str,
    ) -> Result<AuthorizedExecution<'a>, ControlError> {
        if !valid_id(snapshot)
            || node_journal.node_id() != self.identity.node_id
            || journal.node_id() != self.identity.node_id
        {
            return Err(ControlError::Invalid);
        }
        if self.execution_control_version != 2 || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        // Persist uncertainty BEFORE the first send. Re-entering this method
        // cannot recreate its monotonic anchor: Journal rejects ack_sent.
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        let ack = journal
            .send_ack(self.identity(), command)
            .map_err(|_| ControlError::Stale)?;
        let sent = Instant::now();
        let wait_until = self.stop_at.min(sent + Duration::from_secs(6));
        self.active = false;
        let operation = async {
            self.sender
                .send(worker::WorkerFrame {
                    body: Some(worker_frame::Body::Acknowledge(ack.clone())),
                })
                .await
                .map_err(|_| ControlError::Unavailable)?;
            for _ in 0..8 {
                let frame = self
                    .stream
                    .message()
                    .await
                    .map_err(status)?
                    .ok_or(ControlError::Unavailable)?;
                match frame.body {
                    Some(server_frame::Body::Assignment(a)) if a == record.assignment => continue,
                    Some(server_frame::Body::Start(start)) => {
                        if start.command_id != ack.command_id
                            || start.payload_digest != ack.payload_digest
                            || start.lease != ack.lease
                            || start.execution_journal_id != ack.execution_journal_id
                            || start.broker_grant_id != ack.broker_grant_id
                            || start.broker_epoch != ack.broker_epoch
                            || start.slot_generation != ack.slot_generation
                            || !(1001..=45000).contains(&start.start_budget_ms)
                        {
                            return Err(ControlError::Protocol);
                        }
                        return Ok(start);
                    }
                    _ => return Err(ControlError::Stale),
                }
            }
            Err(ControlError::Protocol)
        };
        let start = tokio::time::timeout_at(tokio::time::Instant::from_std(wait_until), operation)
            .await
            .map_err(|_| ControlError::Unavailable)??;
        let deadline = self
            .stop_at
            .min(sent + Duration::from_millis(start.start_budget_ms - 1000));
        if Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        self.active = true;
        let fresh_sent = Instant::now();
        let notices = self.heartbeat_node_control(snapshot, node_journal).await?;
        if !notices.is_empty()
            || self.observed_policy_generation != record.assignment.node_policy_generation
            || Instant::now() >= deadline.min(fresh_sent + Duration::from_secs(5))
        {
            self.active = false;
            return Err(ControlError::Stale);
        }
        Ok(AuthorizedExecution {
            connection: self,
            start,
            sent,
            deadline,
            confirm_before: deadline.min(fresh_sent + Duration::from_secs(5)),
            consumed: false,
        })
    }
}
