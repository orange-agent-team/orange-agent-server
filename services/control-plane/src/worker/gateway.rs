//! mTLS-only Worker control transport and negotiated durable node receipt.
//! Enrollment/model execution/cleanup are separate; unsupported frames fail.
use super::{DeviceSession, SessionError, WorkerSessionStore};
use orange_protocol::grpc::worker::{
    self, server_frame, worker_frame,
    worker_gateway_server::{WorkerGateway, WorkerGatewayServer},
};
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc, watch},
};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Request, Response, Status, Streaming,
    transport::{
        Certificate, Identity, Server, ServerTlsConfig,
        server::{TcpConnectInfo, TlsConnectInfo},
    },
};

pub struct GatewayTls {
    pub server_certificate_pem: Vec<u8>,
    pub server_private_key_pem: Vec<u8>,
    pub client_ca_pem: Vec<u8>,
}
#[derive(Clone)]
struct Gateway {
    store: WorkerSessionStore,
    slots: Arc<Semaphore>,
    input_slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    artifacts: Option<crate::artifacts::ArtifactReader>,
    writer: Option<crate::artifacts::ArtifactStore>,
}
pub async fn serve(
    listener: TcpListener,
    tls: GatewayTls,
    store: WorkerSessionStore,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    serve_with_artifacts(listener, tls, store, None, shutdown).await
}
pub async fn serve_with_artifacts(
    listener: TcpListener,
    tls: GatewayTls,
    store: WorkerSessionStore,
    artifacts: Option<crate::artifacts::ArtifactReader>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    serve_storage(listener, tls, store, artifacts, None, shutdown).await
}
/// Share the existing single writer; never reopen a second writable root.
pub async fn serve_with_output_store(
    listener: TcpListener,
    tls: GatewayTls,
    store: WorkerSessionStore,
    writer: crate::artifacts::ArtifactStore,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    serve_storage(
        listener,
        tls,
        store,
        Some(writer.reader()),
        Some(writer),
        shutdown,
    )
    .await
}
async fn serve_storage(
    listener: TcpListener,
    tls: GatewayTls,
    store: WorkerSessionStore,
    artifacts: Option<crate::artifacts::ArtifactReader>,
    writer: Option<crate::artifacts::ArtifactStore>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    let (stop, receiver) = watch::channel(false);
    let gateway = Gateway {
        store,
        slots: Arc::new(Semaphore::new(128)),
        input_slots: Arc::new(Semaphore::new(16)),
        shutdown: receiver,
        artifacts,
        writer,
    };
    Server::builder()
        .tls_config(
            ServerTlsConfig::new()
                .identity(Identity::from_pem(
                    tls.server_certificate_pem,
                    tls.server_private_key_pem,
                ))
                .client_ca_root(Certificate::from_pem(tls.client_ca_pem))
                .client_auth_optional(false)
                .timeout(Duration::from_secs(5)),
        )?
        .concurrency_limit_per_connection(8)
        .max_concurrent_streams(Some(8))
        .add_service(
            WorkerGatewayServer::new(gateway)
                .max_decoding_message_size(64 * 1024)
                .max_encoding_message_size(64 * 1024),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
            shutdown.await;
            let _ = stop.send(true);
        })
        .await
}
fn timestamp(seconds: i64) -> Option<prost_types::Timestamp> {
    Some(prost_types::Timestamp { seconds, nanos: 0 })
}
fn fingerprint<T>(request: &Request<T>) -> Result<String, Status> {
    let certs = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(TlsConnectInfo::peer_certs)
        .ok_or_else(|| Status::unauthenticated("verified peer required"))?;
    let leaf = certs
        .first()
        .ok_or_else(|| Status::unauthenticated("verified leaf required"))?;
    Ok(hex::encode(Sha256::digest(leaf.as_ref())))
}
fn status(error: SessionError) -> Status {
    match error {
        SessionError::Invalid => Status::invalid_argument("invalid worker control message"),
        SessionError::Unauthorized => {
            Status::unauthenticated("device credential or owner unavailable")
        }
        SessionError::Stale => Status::failed_precondition("control session superseded or expired"),
        SessionError::Database(_) => Status::unavailable("authoritative store unavailable"),
    }
}
fn output_error(error: crate::artifacts::ArtifactError) -> Status {
    use crate::artifacts::ArtifactError as E;
    match error {
        E::Forbidden => Status::permission_denied("output authority unavailable"),
        E::Invalid | E::Input | E::Integrity => {
            Status::invalid_argument("invalid output bytes or projection")
        }
        E::NotFound | E::NotReady => Status::not_found("committed output unavailable"),
        E::Conflict | E::Deleted | E::Expired => {
            Status::failed_precondition("output no longer valid or conflicting")
        }
        E::Busy | E::Capacity => Status::resource_exhausted("output capacity unavailable"),
        _ => Status::unavailable("output storage unavailable"),
    }
}
fn welcome(s: &DeviceSession) -> worker::ServerFrame {
    worker::ServerFrame {
        body: Some(server_frame::Body::Welcome(worker::Welcome {
            session_id: s.session_id.clone(),
            connection_epoch: s.connection_epoch as u64,
            protocol_version: 1,
            expires_at: timestamp(s.expires_at_unix),
            node_control_version: s.node_control_version,
            execution_control_version: s.execution_control_version,
            rejection_receipts_version: s.rejection_receipts_version,
            input_artifacts_version: s.input_artifacts_version,
            renewal_version: s.renewal_version,
            output_version: s.output_version,
            cpu_cleanup_version: s.cpu_cleanup_version,
            node_policy_generation: s.node_policy_generation,
        })),
    }
}
#[tonic::async_trait]
impl WorkerGateway for Gateway {
    type ControlStream = ReceiverStream<Result<worker::ServerFrame, Status>>;
    async fn confirm_cpu_cleanup(
        &self,
        request: Request<worker::CpuCleanupRequest>,
    ) -> Result<Response<worker::CpuCleanupReceipt>, Status> {
        let _permit = self
            .input_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("cleanup request limit"))?;
        if *self.shutdown.borrow() {
            return Err(Status::unavailable("gateway stopping"));
        }
        let peer = fingerprint(&request)?;
        let body = request.into_inner();
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            let session = self
                .store
                .current_peer(&peer, &body.session_id)
                .await
                .map_err(status)?;
            self.store
                .confirm_cpu_cleanup(&session, &body)
                .await
                .map_err(status)
        })
        .await
        .map_err(|_| Status::deadline_exceeded("cleanup confirmation deadline"))??;
        if let Some(writer) = &self.writer {
            writer
                .settle_committed(&result.job_id)
                .await
                .map_err(output_error)?;
        }
        crate::store_cleanup::finalize(&self.store.pool, &result.job_id)
            .await
            .map_err(|_| Status::unavailable("cleanup financial finalization pending"))?;
        Ok(Response::new(result))
    }
    async fn commit_output(
        &self,
        request: Request<Streaming<worker::OutputFrame>>,
    ) -> Result<Response<worker::OutputReceipt>, Status> {
        let _permit = self
            .input_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("output request limit"))?;
        if *self.shutdown.borrow() {
            return Err(Status::unavailable("gateway stopping"));
        }
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| Status::unimplemented("output writer not configured"))?;
        let peer = fingerprint(&request)?;
        let mut stream = request.into_inner();
        let first = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("output header deadline"))??
            .ok_or_else(|| Status::invalid_argument("output header required"))?;
        let Some(worker::output_frame::Body::Header(header)) = first.body else {
            return Err(Status::invalid_argument("header must be first"));
        };
        let session = self
            .store
            .input_peer(&peer, &header.session_id)
            .await
            .map_err(status)?;
        let input = stream.map(|frame| match frame {
            Ok(worker::OutputFrame {
                body: Some(worker::output_frame::Body::Data(data)),
            }) => Ok(data),
            _ => Err(crate::artifacts::ArtifactError::Input),
        });
        let receipt = writer
            .commit_output(self.store.clone(), session, header, input)
            .await
            .map_err(output_error)?;
        // Output commit is already durable. A settlement error leaves pending
        // facts for the internal retry owner; never invent a successful charge.
        writer
            .settle_committed(&receipt.job_id)
            .await
            .map_err(output_error)?;
        Ok(Response::new(receipt))
    }
    async fn query_output(
        &self,
        request: Request<worker::OutputQuery>,
    ) -> Result<Response<worker::OutputReceipt>, Status> {
        let _permit = self
            .input_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("output query limit"))?;
        if *self.shutdown.borrow() {
            return Err(Status::unavailable("gateway stopping"));
        }
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| Status::unimplemented("output writer not configured"))?;
        let peer = fingerprint(&request)?;
        let request = request.into_inner();
        let session = self
            .store
            .input_peer(&peer, &request.session_id)
            .await
            .map_err(status)?;
        let receipt = writer
            .query_output(
                self.store.clone(),
                session,
                request
                    .original
                    .ok_or_else(|| Status::invalid_argument("original output required"))?,
            )
            .await
            .map_err(output_error)?;
        Ok(Response::new(receipt))
    }
    async fn renew_execution(
        &self,
        request: Request<worker::ExecutionRenewalRequest>,
    ) -> Result<Response<worker::ExecutionRenewalReply>, Status> {
        if *self.shutdown.borrow() {
            return Err(Status::unavailable("gateway stopping"));
        }
        let _permit = self
            .input_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("execution request limit"))?;
        let peer = fingerprint(&request)?;
        let body = request.into_inner();
        let operation = async {
            let session = self
                .store
                .input_peer(&peer, &body.session_id)
                .await
                .map_err(status)?;
            self.store
                .renew_execution(&session, &body)
                .await
                .map_err(status)
        };
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .map_err(|_| Status::deadline_exceeded("renewal deadline"))??;
        Ok(Response::new(result))
    }
    async fn read_input(
        &self,
        request: Request<worker::InputReadRequest>,
    ) -> Result<Response<worker::InputReadReply>, Status> {
        let _permit = self
            .input_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("input request limit"))?;
        let objects = self
            .artifacts
            .as_ref()
            .ok_or_else(|| Status::unimplemented("input storage not configured"))?;
        let certs = request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .and_then(TlsConnectInfo::peer_certs)
            .ok_or_else(|| Status::unauthenticated("verified peer required"))?;
        let fingerprint = hex::encode(Sha256::digest(
            certs
                .first()
                .ok_or_else(|| Status::unauthenticated("peer required"))?
                .as_ref(),
        ));
        let body = request.into_inner();
        let operation = async {
            let session = self
                .store
                .input_peer(&fingerprint, &body.session_id)
                .await
                .map_err(status)?;
            objects
                .read_input(self.store.clone(), session, body)
                .await
                .map_err(|e| match e {
                    crate::artifacts::ArtifactError::Forbidden => {
                        Status::permission_denied("input authority unavailable")
                    }
                    crate::artifacts::ArtifactError::Invalid => {
                        Status::invalid_argument("invalid input read")
                    }
                    crate::artifacts::ArtifactError::Busy => {
                        Status::resource_exhausted("input read limit")
                    }
                    _ => Status::failed_precondition("input unavailable or no longer valid"),
                })
        };
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .map_err(|_| Status::deadline_exceeded("input deadline"))??;
        Ok(Response::new(result))
    }

    async fn control(
        &self,
        request: Request<Streaming<worker::WorkerFrame>>,
    ) -> Result<Response<Self::ControlStream>, Status> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("control connection limit"))?;
        let certificates = request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .and_then(TlsConnectInfo::peer_certs)
            .ok_or_else(|| Status::unauthenticated("verified TLS peer certificate required"))?;
        let leaf = certificates
            .first()
            .ok_or_else(|| Status::unauthenticated("peer certificate required"))?;
        let fingerprint = hex::encode(Sha256::digest(leaf.as_ref()));
        let mut stream = request.into_inner();
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return Err(Status::unavailable("gateway stopping"));
        }
        let first = tokio::select! {
            _=shutdown.changed()=>return Err(Status::unavailable("gateway stopping")),
            frame=tokio::time::timeout(Duration::from_secs(5),stream.message())=>frame.map_err(|_|Status::deadline_exceeded("Hello deadline"))??.ok_or_else(||Status::invalid_argument("Hello required"))?,
        };
        let Some(worker_frame::Body::Hello(hello)) = first.body else {
            return Err(Status::invalid_argument("first frame must be Hello"));
        };
        if hello.protocol_version != 1
            || hello.node_control_version > 2
            || hello.execution_control_version > 2
            || hello.rejection_receipts_version > 1
            || hello.input_artifacts_version > 1
            || hello.renewal_version > 1
            || hello.output_version > 1
            || hello.cpu_cleanup_version > 1
            || (hello.output_version == 1 && self.writer.is_none())
            || (hello.input_artifacts_version == 1 && self.artifacts.is_none())
        {
            return Err(Status::failed_precondition("unsupported protocol version"));
        }
        let session = tokio::time::timeout(
            Duration::from_secs(5),
            self.store.claim(&hello, &fingerprint),
        )
        .await
        .map_err(|_| Status::unavailable("authoritative store timeout"))?
        .map_err(status)?;
        let (sender, receiver) = mpsc::channel(8);
        sender
            .send(Ok(welcome(&session)))
            .await
            .map_err(|_| Status::cancelled("stream closed"))?;
        let store = self.store.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut session = session;
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let result = tokio::select! {
                    _=sender.closed()=>break,
                    _=shutdown.changed()=>Err(Status::unavailable("gateway stopping")),
                    _=interval.tick()=>{
                        let operation=async {
                            store.check(&session).await?;
                            if let Some(command) = store.next_node_command(&session).await? {
                                Ok(Some(worker::ServerFrame { body: Some(server_frame::Body::NodeCommand(command)) }))
                            } else {
                                store.next_execution(&session).await.map(|assignment| assignment.map(|value| worker::ServerFrame {body:Some(server_frame::Body::Assignment(value))}))
                            }
                        };
                        match tokio::time::timeout(Duration::from_secs(5),operation).await{
                            Ok(result)=>result.map_err(status),Err(_)=>Err(Status::unavailable("authoritative store timeout")),
                        }
                    }
                    frame=stream.message()=>{
                        match frame{
                            Ok(Some(frame))=>{
                                match tokio::time::timeout(Duration::from_secs(5),handle(&store,&mut session,frame)).await{
                                    Ok(result)=>result,Err(_)=>Err(Status::unavailable("authoritative store timeout")),
                                }
                            }
                            Ok(None)=>break,Err(error)=>Err(error),
                        }
                    }
                };
                match result {
                    Ok(Some(frame)) => {
                        if !matches!(
                            tokio::time::timeout(Duration::from_secs(2), sender.send(Ok(frame)))
                                .await,
                            Ok(Ok(()))
                        ) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = tokio::time::timeout(
                            Duration::from_millis(200),
                            sender.send(Err(error)),
                        )
                        .await;
                        break;
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), store.disconnect(&session)).await;
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
async fn handle(
    store: &WorkerSessionStore,
    session: &mut DeviceSession,
    frame: worker::WorkerFrame,
) -> Result<Option<worker::ServerFrame>, Status> {
    match frame.body {
        Some(worker_frame::Body::RejectionQuery(query)) => {
            let receipt = store
                .query_rejection(session, &query)
                .await
                .map_err(status)?;
            Ok(Some(worker::ServerFrame {
                body: Some(server_frame::Body::RejectionReceipt(receipt)),
            }))
        }
        Some(worker_frame::Body::Acknowledge(ack)) => {
            let grant = store
                .acknowledge_execution(session, &ack)
                .await
                .map_err(status)?;
            Ok(grant.map(|value| worker::ServerFrame { body: Some(value) }))
        }
        Some(worker_frame::Body::NodeReceipt(receipt)) => {
            store
                .receive_node_receipt(session, &receipt)
                .await
                .map_err(status)?;
            Ok(None)
        }
        Some(worker_frame::Body::Heartbeat(heartbeat)) => {
            *session = store
                .heartbeat(
                    session,
                    &heartbeat.node_id,
                    heartbeat.connection_epoch,
                    &heartbeat.capability_snapshot_id,
                )
                .await
                .map_err(status)?;
            Ok(Some(worker::ServerFrame {
                body: Some(server_frame::Body::Heartbeat(worker::HeartbeatAck {
                    node_id: session.node_id.clone(),
                    connection_epoch: session.connection_epoch as u64,
                    session_id: session.session_id.clone(),
                    expires_at: timestamp(session.expires_at_unix),
                    node_policy_generation: session.node_policy_generation,
                })),
            }))
        }
        None => Err(Status::invalid_argument("unknown or empty control frame")),
        Some(worker_frame::Body::Hello(_)) => {
            Err(Status::invalid_argument("Hello is only valid once"))
        }
        Some(_) => {
            store.check(session).await.map_err(status)?;
            Err(Status::unimplemented(
                "execution frame not supported by this candidate",
            ))
        }
    }
}
