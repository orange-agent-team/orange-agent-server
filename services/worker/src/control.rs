//! Authenticated control liveness. A connected session is never an execution lease.
use crate::broker::{AppliedNodePolicy, Broker, BrokerError};
use crate::node_journal::{NodeJournal, ReceivedNodeIntent};
use orange_protocol::node_control::{NodeStopIntent, StopKind};
use orange_protocol::{
    grpc::worker::{self, server_frame, worker_frame, worker_gateway_client::WorkerGatewayClient},
    valid_digest, valid_id,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Streaming,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity},
};

pub struct ControlConfig {
    pub endpoint: String,
    pub server_name: String,
    pub server_ca_pem: Vec<u8>,
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub node_id: String,
    pub capabilities_digest: String,
}
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("invalid control configuration or frame")]
    Invalid,
    #[error("control transport unavailable")]
    Unavailable,
    #[error("device authorization revoked or rejected")]
    Unauthorized,
    #[error("control session superseded or expired")]
    Stale,
    #[error("server requested unsupported execution behavior; no acknowledgement sent")]
    Unsupported,
    #[error("control protocol mismatch")]
    Protocol,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlIdentity {
    pub node_id: String,
    pub session_id: String,
    pub connection_epoch: u64,
}
pub struct ControlConnection {
    identity: ControlIdentity,
    sender: mpsc::Sender<worker::WorkerFrame>,
    stream: Streaming<worker::ServerFrame>,
    stop_at: Instant,
    active: bool,
    node_control_version: u32,
    observed_policy_generation: u64,
}
/// Short-lived, nonserializable authorization borrowing its live control session.
/// Acquiring it performs network I/O without holding a Broker lock. Applying it
/// changes only persistent remote policy; resource cleanup remains separate.
pub struct AuthorizedNodeStop<'a> {
    connection: &'a mut ControlConnection,
    received: ReceivedNodeIntent,
    deadline: Instant,
}
impl AuthorizedNodeStop<'_> {
    pub fn apply(self, broker: &mut Broker) -> Result<AppliedNodePolicy, ControlError> {
        if !self.connection.is_current_locally() || Instant::now() >= self.deadline {
            return Err(ControlError::Stale);
        }
        broker
            .apply_verified_node_stop(&self.received, self.deadline)
            .map_err(|error| match error {
                BrokerError::Unauthorized => ControlError::Unauthorized,
                BrokerError::Stale | BrokerError::Conflict | BrokerError::RecoveryRequired => {
                    ControlError::Stale
                }
                _ => ControlError::Unavailable,
            })
    }
}
impl ControlConnection {
    pub async fn connect(config: ControlConfig) -> Result<Self, ControlError> {
        Self::connect_mode(config, 0).await
    }
    pub async fn connect_node_control(
        config: ControlConfig,
        journal: &mut NodeJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_journal(config, journal, 1).await
    }
    pub async fn connect_node_policy(
        config: ControlConfig,
        journal: &mut NodeJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_journal(config, journal, 2).await
    }
    async fn connect_journal(
        config: ControlConfig,
        journal: &mut NodeJournal,
        version: u32,
    ) -> Result<Self, ControlError> {
        if config.node_id != journal.node_id() {
            return Err(ControlError::Invalid);
        }
        let connection = Self::connect_mode(config, version).await?;
        journal
            .bind(connection.identity())
            .map_err(|_| ControlError::Unavailable)?;
        Ok(connection)
    }
    async fn connect_mode(
        config: ControlConfig,
        node_control_version: u32,
    ) -> Result<Self, ControlError> {
        if !valid_id(&config.node_id)
            || !valid_digest(&config.capabilities_digest)
            || config.server_name.is_empty()
            || config.server_name.len() > 253
            || [
                &config.server_ca_pem,
                &config.certificate_pem,
                &config.private_key_pem,
            ]
            .iter()
            .any(|v| v.is_empty() || v.len() > 64 * 1024)
        {
            return Err(ControlError::Invalid);
        }
        let endpoint = Endpoint::from_shared(config.endpoint).map_err(|_| ControlError::Invalid)?;
        let uri = endpoint.uri();
        if uri.scheme_str() != Some("https")
            || uri.path() != "/"
            || uri.query().is_some()
            || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        {
            return Err(ControlError::Invalid);
        }
        let tls = ClientTlsConfig::new()
            .domain_name(config.server_name)
            .ca_certificate(Certificate::from_pem(config.server_ca_pem))
            .identity(Identity::from_pem(
                config.certificate_pem,
                config.private_key_pem,
            ));
        let channel = endpoint
            .connect_timeout(Duration::from_secs(5))
            .tls_config(tls)
            .map_err(|_| ControlError::Invalid)?
            .connect()
            .await
            .map_err(|_| ControlError::Unavailable)?;
        Self::establish(
            channel,
            config.node_id,
            config.capabilities_digest,
            node_control_version,
        )
        .await
    }
    async fn establish(
        channel: Channel,
        node: String,
        capabilities: String,
        node_control_version: u32,
    ) -> Result<Self, ControlError> {
        let mut client = WorkerGatewayClient::new(channel)
            .max_decoding_message_size(64 * 1024)
            .max_encoding_message_size(64 * 1024);
        let (sender, receiver) = mpsc::channel(8);
        let sent = Instant::now();
        sender
            .send(worker::WorkerFrame {
                body: Some(worker_frame::Body::Hello(worker::Hello {
                    node_id: node.clone(),
                    protocol_version: 1,
                    capabilities_digest: capabilities,
                    node_control_version,
                })),
            })
            .await
            .map_err(|_| ControlError::Unavailable)?;
        let mut stream = tokio::time::timeout(
            Duration::from_secs(6),
            client.control(ReceiverStream::new(receiver)),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        let frame = tokio::time::timeout(Duration::from_secs(6), stream.message())
            .await
            .map_err(|_| ControlError::Unavailable)?
            .map_err(status)?
            .ok_or(ControlError::Unavailable)?;
        let Some(server_frame::Body::Welcome(welcome)) = frame.body else {
            return Err(ControlError::Protocol);
        };
        if welcome.protocol_version != 1
            || !valid_id(&welcome.session_id)
            || welcome.connection_epoch == 0
            || welcome.connection_epoch > i64::MAX as u64
            || welcome
                .expires_at
                .is_none_or(|t| t.seconds <= 0 || !(0..1_000_000_000).contains(&t.nanos))
        {
            return Err(ControlError::Protocol);
        }
        if welcome.node_control_version != node_control_version {
            return Err(ControlError::Unsupported);
        }
        if node_control_version == 2 && welcome.node_policy_generation > i64::MAX as u64 {
            return Err(ControlError::Protocol);
        }
        // Protocol v1 control sessions have 45s TTL. Subtract transport latency
        // by starting from send time, and retain a one-second safety margin.
        let stop_at = sent + Duration::from_secs(44);
        if stop_at <= Instant::now() {
            return Err(ControlError::Stale);
        }
        Ok(Self {
            identity: ControlIdentity {
                node_id: node,
                session_id: welcome.session_id,
                connection_epoch: welcome.connection_epoch,
            },
            sender,
            stream,
            stop_at,
            active: true,
            node_control_version,
            observed_policy_generation: welcome.node_policy_generation,
        })
    }
    pub fn identity(&self) -> &ControlIdentity {
        &self.identity
    }
    pub fn is_current_locally(&self) -> bool {
        self.active && Instant::now() < self.stop_at
    }
    /// No execution grant is returned or renewed here. On any uncertain outcome
    /// this connection is unusable until a fresh authenticated handshake.
    pub async fn heartbeat(&mut self, capability_snapshot: &str) -> Result<(), ControlError> {
        if self.node_control_version != 0 {
            return Err(ControlError::Unsupported);
        }
        self.heartbeat_inner(capability_snapshot, None)
            .await
            .map(|_| ())
    }
    /// Returned records prove local durable receipt only, not server acceptance,
    /// Broker policy application, process exit, or device cleanup. No automatic
    /// effect replay occurs on reconnect or after an uncertain result.
    pub async fn heartbeat_node_control(
        &mut self,
        capability_snapshot: &str,
        journal: &mut NodeJournal,
    ) -> Result<Vec<ReceivedNodeIntent>, ControlError> {
        if !matches!(self.node_control_version, 1 | 2) || journal.node_id() != self.identity.node_id
        {
            return Err(ControlError::Unsupported);
        }
        self.heartbeat_inner(capability_snapshot, Some(journal))
            .await
    }
    pub async fn authorize_node_stop<'a>(
        &'a mut self,
        capability_snapshot: &str,
        journal: &mut NodeJournal,
        command: &str,
    ) -> Result<AuthorizedNodeStop<'a>, ControlError> {
        if self.node_control_version != 2 {
            return Err(ControlError::Unsupported);
        }
        if !valid_id(command) {
            return Err(ControlError::Invalid);
        }
        self.heartbeat_node_control(capability_snapshot, journal)
            .await?;
        let received = journal
            .current_receipt(&self.identity, command)
            .map_err(|_| ControlError::Stale)?;
        if received.intent.generation().map(|g| g as u64) != Some(self.observed_policy_generation) {
            return Err(ControlError::Stale);
        }
        let deadline = self.stop_at.min(Instant::now() + Duration::from_secs(5));
        Ok(AuthorizedNodeStop {
            connection: self,
            received,
            deadline,
        })
    }
    async fn heartbeat_inner(
        &mut self,
        capability_snapshot: &str,
        mut journal: Option<&mut NodeJournal>,
    ) -> Result<Vec<ReceivedNodeIntent>, ControlError> {
        if !self.is_current_locally() {
            self.active = false;
            return Err(ControlError::Stale);
        }
        if !valid_id(capability_snapshot) {
            return Err(ControlError::Invalid);
        }
        let sent = Instant::now();
        let deadline = self.stop_at.min(sent + Duration::from_secs(6));
        // Set before await so cancellation/dropped Future cannot retain authority.
        self.active = false;
        let operation = async {
            self.sender
                .send(worker::WorkerFrame {
                    body: Some(worker_frame::Body::Heartbeat(worker::Heartbeat {
                        node_id: self.identity.node_id.clone(),
                        connection_epoch: self.identity.connection_epoch,
                        capability_snapshot_id: capability_snapshot.into(),
                    })),
                })
                .await
                .map_err(|_| ControlError::Unavailable)?;
            let mut received = Vec::new();
            loop {
                let frame = self
                    .stream
                    .message()
                    .await
                    .map_err(status)?
                    .ok_or(ControlError::Unavailable)?;
                if let Some(server_frame::Body::NodeCommand(command)) = &frame.body {
                    if !matches!(self.node_control_version, 1 | 2)
                        || received.len() >= 8
                        || command.node_id != self.identity.node_id
                        || command.session_id != self.identity.session_id
                        || command.connection_epoch != self.identity.connection_epoch
                        || Instant::now() >= deadline
                        || (self.node_control_version == 2
                            && command.policy_generation < self.observed_policy_generation)
                    {
                        return Err(ControlError::Protocol);
                    }
                    let kind = match worker::NodeStopKind::try_from(command.kind) {
                        Ok(worker::NodeStopKind::Drain) => StopKind::Drain,
                        Ok(worker::NodeStopKind::Quarantine) => StopKind::Quarantine,
                        _ => return Err(ControlError::Protocol),
                    };
                    let intent = NodeStopIntent {
                        command_id: command.command_id.clone(),
                        action_id: command.action_id.clone(),
                        node_id: command.node_id.clone(),
                        policy_generation: command.policy_generation.to_string(),
                        kind,
                    };
                    let store = journal.as_deref_mut().ok_or(ControlError::Unsupported)?;
                    let receipt = store
                        .receive(&self.identity, &intent, &command.payload_digest)
                        .map_err(|_| ControlError::Unavailable)?;
                    if Instant::now() >= deadline {
                        return Err(ControlError::Stale);
                    }
                    if self.node_control_version == 2 {
                        self.observed_policy_generation = command.policy_generation;
                    }
                    self.sender
                        .send(worker::WorkerFrame {
                            body: Some(worker_frame::Body::NodeReceipt(worker::NodeReceipt {
                                command_id: command.command_id.clone(),
                                node_id: self.identity.node_id.clone(),
                                policy_generation: command.policy_generation,
                                payload_digest: command.payload_digest.clone(),
                                receipt_id: receipt.receipt_id.clone(),
                                session_id: self.identity.session_id.clone(),
                                connection_epoch: self.identity.connection_epoch,
                            })),
                        })
                        .await
                        .map_err(|_| ControlError::Unavailable)?;
                    received.push(receipt);
                    continue;
                }
                let Some(server_frame::Body::Heartbeat(ack)) = frame.body else {
                    return Err(ControlError::Unsupported);
                };
                if ack.node_id != self.identity.node_id
                    || ack.session_id != self.identity.session_id
                    || ack.connection_epoch != self.identity.connection_epoch
                    || ack
                        .expires_at
                        .is_none_or(|t| t.seconds <= 0 || !(0..1_000_000_000).contains(&t.nanos))
                {
                    return Err(ControlError::Protocol);
                }
                if self.node_control_version == 2 {
                    if ack.node_policy_generation > i64::MAX as u64
                        || ack.node_policy_generation < self.observed_policy_generation
                    {
                        return Err(ControlError::Stale);
                    }
                    self.observed_policy_generation = ack.node_policy_generation;
                }
                return Ok(received);
            }
        };
        let received = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .map_err(|_| ControlError::Unavailable)??;
        self.stop_at = sent + Duration::from_secs(44);
        self.active = true;
        Ok(received)
    }
    /// Transport close only. Caller must separately drain any future execution.
    pub fn close(self) {
        drop(self);
    }
}
fn status(status: tonic::Status) -> ControlError {
    match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => ControlError::Unauthorized,
        tonic::Code::FailedPrecondition => ControlError::Stale,
        tonic::Code::Unimplemented => ControlError::Unsupported,
        tonic::Code::InvalidArgument => ControlError::Protocol,
        _ => ControlError::Unavailable,
    }
}
