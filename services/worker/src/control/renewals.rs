use super::*;
use crate::{
    broker::{Grant, RemoteBinding, Supervisor, VerifiedCloudGrant},
    execution_journal::{ExecutionJournal, ExecutionRecord},
};
use orange_protocol::execution::{renewal_digest, seconds};

/// One durable request with its original monotonic anchor; deliberately not Clone
/// or deserializable. Dropped/uncertain requests cannot be sent as fresh grants.
pub struct PendingRenewal {
    request: worker::ExecutionRenewalRequest,
    digest: String,
    sent: Instant,
    grant: Grant,
    job_deadline: i64,
}
pub struct VerifiedRenewal {
    request: PendingRenewal,
    reply: worker::ExecutionRenewalReply,
    cloud: VerifiedCloudGrant,
    stop_at: Instant,
}
impl ControlConnection {
    pub async fn connect_renewable_execution(
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
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub fn supports_renewal(&self) -> bool {
        self.renewal_version == 1
    }
    pub fn prepare_renewal(
        &self,
        journal: &mut ExecutionJournal,
        command: &str,
    ) -> Result<PendingRenewal, ControlError> {
        if !self.supports_renewal() || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if record.state != "running" || record.assignment.session_id != self.identity.session_id {
            return Err(ControlError::Stale);
        }
        let grant = record.grant.clone().ok_or(ControlError::Stale)?;
        let sequence = journal
            .next_renewal(self.identity(), command)
            .map_err(|_| ControlError::Stale)?;
        let request = worker::ExecutionRenewalRequest {
            session_id: self.identity.session_id.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            sequence,
            accepted: Some(record.ack().map_err(|_| ControlError::Stale)?),
            supervisor_instance_id: grant.supervisor_instance.clone(),
        };
        let digest = renewal_digest(&request, &record.assignment).ok_or(ControlError::Invalid)?;
        // Anchor before the durable write and network send, never receive time.
        let sent = Instant::now();
        journal
            .begin_renewal(command, &request, &digest)
            .map_err(|_| ControlError::Unavailable)?;
        Ok(PendingRenewal {
            request,
            digest,
            sent,
            grant,
            job_deadline: seconds(&record.assignment.job_deadline).ok_or(ControlError::Invalid)?,
        })
    }
    pub async fn renew_execution(
        &mut self,
        pending: PendingRenewal,
    ) -> Result<VerifiedRenewal, ControlError> {
        if !self.supports_renewal()
            || !self.is_current_locally()
            || pending.request.session_id != self.identity.session_id
        {
            return Err(ControlError::Stale);
        }
        let deadline = self.stop_at.min(pending.sent + Duration::from_secs(5));
        self.active = false;
        let reply = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.input_client.renew_execution(pending.request.clone()),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        if reply.request_id != pending.request.request_id
            || reply.sequence != pending.request.sequence
            || reply.request_digest != pending.digest
            || !(2000..=45000).contains(&reply.budget_ms)
            || seconds(&reply.expires_at).is_none_or(|v| v > pending.job_deadline)
            || Instant::now() >= deadline
        {
            return Err(ControlError::Protocol);
        }
        let ack = pending
            .request
            .accepted
            .as_ref()
            .ok_or(ControlError::Invalid)?;
        let lease = ack.lease.as_ref().ok_or(ControlError::Invalid)?;
        let stop_at =
            pending.sent + Duration::from_millis(reply.budget_ms) - Duration::from_secs(1);
        let cloud = VerifiedCloudGrant::from_verified_start(
            RemoteBinding {
                node_id: self.identity.node_id.clone(),
                attempt_id: lease.attempt_id.clone(),
                lease_id: lease.lease_id.clone(),
                connection_epoch: lease.connection_epoch,
                fencing_token: lease.fencing_token,
            },
            &pending.grant,
            pending.sent,
            Duration::from_millis(reply.budget_ms),
        )
        .map_err(|_| ControlError::Stale)?;
        self.active = true;
        Ok(VerifiedRenewal {
            request: pending,
            reply,
            cloud,
            stop_at,
        })
    }
    pub(crate) fn confirm_input_renewal(
        &mut self,
        journal: &mut ExecutionJournal,
        broker: &mut Broker,
        supervisor: &Supervisor,
        started: &mut super::execution::StartedExecution,
        renewal: VerifiedRenewal,
    ) -> Result<(), ControlError> {
        if &renewal.request.grant != started.grant() || Instant::now() >= started.stop_at() {
            return Err(ControlError::Stale);
        }
        let deadline = renewal.stop_at.min(self.stop_at);
        self.confirm_renewal(journal, broker, supervisor, renewal)?;
        started.renew_until(deadline);
        Ok(())
    }
    pub fn confirm_renewal(
        &mut self,
        journal: &mut ExecutionJournal,
        broker: &mut Broker,
        supervisor: &Supervisor,
        renewal: VerifiedRenewal,
    ) -> Result<(), ControlError> {
        if !self.is_current_locally()
            || renewal.request.request.session_id != self.identity.session_id
            || Instant::now() >= renewal.stop_at
        {
            return Err(ControlError::Stale);
        }
        let grant = &renewal.request.grant;
        let ack = renewal
            .request
            .request
            .accepted
            .as_ref()
            .ok_or(ControlError::Invalid)?;
        let record: ExecutionRecord = journal
            .record(&ack.command_id)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Stale)?;
        if record.state != "running" || record.grant.as_ref() != Some(grant) {
            return Err(ControlError::Stale);
        }
        journal
            .confirm_renewal(&renewal.request.request, &renewal.reply)
            .map_err(|_| ControlError::Unavailable)?;
        let local_stop = renewal
            .stop_at
            .min(Instant::now() + Duration::from_secs(14));
        broker
            .renew(supervisor, grant, Some(&renewal.cloud))
            .map_err(|_| ControlError::Stale)?;
        if Instant::now() >= local_stop {
            self.active = false;
            broker
                .begin_drain(supervisor, grant)
                .map_err(|_| ControlError::Unavailable)?;
            return Err(ControlError::Stale);
        }
        Ok(())
    }
}
