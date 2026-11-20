#[path = "worker_sessions/execution_cases.rs"]
mod execution_cases;
#[path = "worker_sessions/inputs.rs"]
mod input_cases;
#[path = "worker_sessions/policy_cases.rs"]
mod policy_cases;
#[path = "worker_sessions/receiver_cases.rs"]
mod receiver_cases;
#[cfg(all(unix, feature = "transcription-observation-tests"))]
#[path = "worker_sessions/transcription_observation.rs"]
mod transcription_observation;
use orange_control_plane::{
    store::Store,
    worker::{
        WorkerSessionStore,
        gateway::{self, GatewayTls},
    },
};
use orange_protocol::grpc::worker::{
    self, server_frame, worker_frame, worker_gateway_client::WorkerGatewayClient,
};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Streaming,
    transport::{Certificate, Channel, ClientTlsConfig, Identity},
};
use uuid::Uuid;
fn file(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("test CA directory required"))
            .join(name),
    )
    .expect("test certificate")
}
struct Device {
    store: Store,
    node: String,
    owner: String,
    user: String,
    certificate: String,
    fingerprint: String,
}
async fn device(certificate: &str) -> Device {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("real PostgreSQL required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let owner = Uuid::new_v4().to_string();
    let user = Uuid::new_v4().to_string();
    let node = Uuid::new_v4().to_string();
    let fingerprint = hex::encode(Sha256::digest(file(&format!("{certificate}.der"))));
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("owner fixture");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'worker-session-test',$1,$2)").bind(&user).bind(&owner).execute(store.pool()).await.expect("user fixture");
    sqlx::query("INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES($1,$2,'dev','local-test','offline')").bind(&node).bind(&owner).execute(store.pool()).await.expect("node fixture");
    sqlx::query("INSERT INTO worker_credentials(certificate_sha256,node_id,enrollment_user_id,not_before,expires_at) VALUES($1,$2,$3,clock_timestamp()-interval '1 minute',clock_timestamp()+interval '1 hour')").bind(&fingerprint).bind(&node).bind(&user).execute(store.pool()).await.expect("explicit certificate provision fixture");
    Device {
        store,
        node,
        owner,
        user,
        certificate: certificate.into(),
        fingerprint,
    }
}
struct Running {
    port: u16,
    stop: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), tonic::transport::Error>>,
}
impl Running {
    async fn start(store: &Store) -> Self {
        Self::start_with_input(store, None).await
    }
    async fn start_with_input(
        store: &Store,
        objects: Option<orange_control_plane::artifacts::ArtifactReader>,
    ) -> Self {
        Self::start_storage(store, objects, None).await
    }
    async fn start_with_output(
        store: &Store,
        writer: orange_control_plane::artifacts::ArtifactStore,
    ) -> Self {
        Self::start_storage(store, None, Some(writer)).await
    }
    async fn start_storage(
        store: &Store,
        objects: Option<orange_control_plane::artifacts::ArtifactReader>,
        writer: Option<orange_control_plane::artifacts::ArtifactStore>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let tls = GatewayTls {
            server_certificate_pem: file("server.pem"),
            server_private_key_pem: file("server.key"),
            client_ca_pem: file("ca.pem"),
        };
        let sessions = WorkerSessionStore::new(store.pool().clone(), Uuid::new_v4().to_string())
            .expect("gateway");
        let (stop, receiver) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let shutdown = async {
                let _ = receiver.await;
            };
            if let Some(writer) = writer {
                gateway::serve_with_output_store(listener, tls, sessions, writer, shutdown).await
            } else {
                gateway::serve_with_artifacts(listener, tls, sessions, objects, shutdown).await
            }
        });
        Self { port, stop, handle }
    }
    async fn client(
        &self,
        certificate: Option<&str>,
    ) -> Result<WorkerGatewayClient<Channel>, tonic::transport::Error> {
        let mut tls = ClientTlsConfig::new()
            .domain_name("localhost")
            .ca_certificate(Certificate::from_pem(file("ca.pem")));
        if let Some(name) = certificate {
            tls = tls.identity(Identity::from_pem(
                file(&format!("{name}.pem")),
                file(&format!("{name}.key")),
            ));
        }
        let channel = Channel::from_shared(format!("https://127.0.0.1:{}", self.port))
            .expect("url")
            .connect_timeout(Duration::from_secs(3))
            .tls_config(tls)?
            .connect()
            .await?;
        Ok(WorkerGatewayClient::new(channel))
    }
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(8), self.handle)
            .await
            .expect("shutdown deadline")
            .expect("task")
            .expect("server");
    }
}
fn hello(node: &str) -> worker::WorkerFrame {
    worker::WorkerFrame {
        body: Some(worker_frame::Body::Hello(worker::Hello {
            node_id: node.into(),
            protocol_version: 1,
            capabilities_digest: "a".repeat(64),
            node_control_version: 0,
            execution_control_version: 0,
            rejection_receipts_version: 0,
            input_artifacts_version: 0,
            renewal_version: 0,
            output_version: 0,
            cpu_cleanup_version: 0,
        })),
    }
}
async fn control(
    client: &mut WorkerGatewayClient<Channel>,
    frame: worker::WorkerFrame,
) -> Result<
    (
        mpsc::Sender<worker::WorkerFrame>,
        Streaming<worker::ServerFrame>,
    ),
    tonic::Status,
> {
    let (sender, receiver) = mpsc::channel(8);
    sender.send(frame).await.expect("first frame");
    let stream = tokio::time::timeout(
        Duration::from_secs(6),
        client.control(ReceiverStream::new(receiver)),
    )
    .await
    .expect("RPC deadline")?
    .into_inner();
    Ok((sender, stream))
}
async fn welcome(stream: &mut Streaming<worker::ServerFrame>) -> worker::Welcome {
    let frame = tokio::time::timeout(Duration::from_secs(4), stream.message())
        .await
        .expect("welcome timeout")
        .expect("welcome transport")
        .expect("welcome frame");
    match frame.body {
        Some(server_frame::Body::Welcome(value)) => value,
        other => panic!("unexpected body: {other:?}"),
    }
}
async fn heartbeat(
    sender: &mpsc::Sender<worker::WorkerFrame>,
    stream: &mut Streaming<worker::ServerFrame>,
    node: &str,
    w: &worker::Welcome,
) -> Result<worker::HeartbeatAck, tonic::Status> {
    let _ = sender
        .send(worker::WorkerFrame {
            body: Some(worker_frame::Body::Heartbeat(worker::Heartbeat {
                node_id: node.into(),
                connection_epoch: w.connection_epoch,
                capability_snapshot_id: "snapshot-test".into(),
            })),
        })
        .await;
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("heartbeat timeout")?
        .ok_or_else(|| tonic::Status::cancelled("stream closed"))?;
    match frame.body {
        Some(server_frame::Body::Heartbeat(ack)) => Ok(ack),
        _ => panic!("heartbeat must not be a start/execution grant"),
    }
}
#[tokio::test]
async fn verified_certificate_binds_node_and_heartbeat_only_renews_control() {
    let d = device("client-a").await;
    sqlx::query("UPDATE nodes SET policy_generation=7 WHERE node_id=$1")
        .bind(&d.node)
        .execute(d.store.pool())
        .await
        .expect("existing policy generation fixture");
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    assert!(control(&mut client, hello("another-node")).await.is_err());
    let (sender, mut stream) = control(&mut client, hello(&d.node)).await.expect("control");
    let w = welcome(&mut stream).await;
    assert!(w.connection_epoch > 0);
    assert_eq!(w.protocol_version, 1);
    assert!(w.expires_at.is_some());
    let ack = heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("heartbeat");
    assert_eq!(ack.session_id, w.session_id);
    assert_eq!(ack.connection_epoch, w.connection_epoch);
    let snapshot: Option<String> = sqlx::query_scalar(
        "SELECT reported_capability_snapshot_id FROM worker_sessions WHERE node_id=$1",
    )
    .bind(&d.node)
    .fetch_one(d.store.pool())
    .await
    .expect("DB");
    assert_eq!(snapshot.as_deref(), Some("snapshot-test"));
    let policy_generation: i64 =
        sqlx::query_scalar("SELECT policy_generation FROM nodes WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("policy generation independent of reconnect");
    assert_eq!(policy_generation, 7);
    sender
        .send(worker::WorkerFrame {
            body: Some(worker_frame::Body::Completion(worker::Completion::default())),
        })
        .await
        .expect("unsupported execution frame");
    assert_eq!(
        stream.message().await.expect_err("no fake success").code(),
        tonic::Code::Unimplemented
    );
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn absent_foreign_and_unregistered_certificates_never_claim_session() {
    let d = device("client-b").await;
    let server = Running::start(&d.store).await;
    for certificate in [None, Some("wrong-ca"), Some("unregistered")] {
        if let Ok(mut client) = server.client(certificate).await {
            assert!(
                control(&mut client, hello(&d.node)).await.is_err(),
                "certificate must authenticate before claiming"
            );
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM worker_sessions WHERE node_id=$1")
        .bind(&d.node)
        .fetch_one(d.store.pool())
        .await
        .expect("count");
    assert_eq!(count, 0);
    server.stop().await;
}
#[tokio::test]
async fn two_gateways_compete_and_old_disconnect_cannot_close_new_connection() {
    let d = device("client-c").await;
    let a = Running::start(&d.store).await;
    let b = Running::start(&d.store).await;
    let mut ca = a.client(Some(&d.certificate)).await.expect("a");
    let mut cb = b.client(Some(&d.certificate)).await.expect("b");
    let (sa, mut ra) = control(&mut ca, hello(&d.node)).await.expect("a control");
    let wa = welcome(&mut ra).await;
    let (sb, mut rb) = control(&mut cb, hello(&d.node)).await.expect("b control");
    let wb = welcome(&mut rb).await;
    assert!(wb.connection_epoch > wa.connection_epoch);
    assert!(heartbeat(&sa, &mut ra, &d.node, &wa).await.is_err());
    drop(sa);
    drop(ra);
    a.stop().await;
    let ack = heartbeat(&sb, &mut rb, &d.node, &wb)
        .await
        .expect("new control persists");
    assert_eq!(ack.session_id, wb.session_id);
    let epoch: i64 =
        sqlx::query_scalar("SELECT connection_epoch FROM worker_sessions WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(d.store.pool())
            .await
            .expect("epoch");
    assert_eq!(epoch, wb.connection_epoch as i64);
    assert!(
        sqlx::query("DELETE FROM worker_session_audit WHERE node_id=$1")
            .bind(&d.node)
            .execute(d.store.pool())
            .await
            .is_err()
    );
    drop(sb);
    drop(rb);
    b.stop().await;
}
#[tokio::test]
async fn certificate_revocation_is_checked_on_existing_stream_and_reconnect() {
    let d = device("client-d").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, hello(&d.node)).await.expect("control");
    let w = welcome(&mut stream).await;
    sqlx::query("UPDATE worker_credentials SET active=false WHERE certificate_sha256=$1")
        .bind(&d.fingerprint)
        .execute(d.store.pool())
        .await
        .expect("revoke");
    assert!(heartbeat(&sender, &mut stream, &d.node, &w).await.is_err());
    assert!(control(&mut client, hello(&d.node)).await.is_err());
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn disabled_user_or_restricted_owner_cannot_keep_stream_authority() {
    for (certificate, disable_user) in [("client-e", true), ("client-f", false)] {
        let d = device(certificate).await;
        let server = Running::start(&d.store).await;
        let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
        let (sender, mut stream) = control(&mut client, hello(&d.node)).await.expect("control");
        let w = welcome(&mut stream).await;
        if disable_user {
            sqlx::query("UPDATE users SET active=false WHERE user_id=$1")
                .bind(&d.user)
                .execute(d.store.pool())
                .await
                .expect("disable");
        } else {
            sqlx::query("UPDATE owners SET state='restricted' WHERE owner_id=$1")
                .bind(&d.owner)
                .execute(d.store.pool())
                .await
                .expect("restrict");
        }
        assert!(heartbeat(&sender, &mut stream, &d.node, &w).await.is_err());
        assert!(control(&mut client, hello(&d.node)).await.is_err());
        drop(sender);
        drop(stream);
        server.stop().await;
    }
}
#[tokio::test]
async fn protocol_mismatch_and_expired_session_cannot_reactivate_with_heartbeat() {
    let d = device("client-g").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let mut frame = hello(&d.node);
    if let Some(worker_frame::Body::Hello(ref mut h)) = frame.body {
        h.protocol_version = 99;
    }
    assert_eq!(
        control(&mut client, frame)
            .await
            .expect_err("unsupported")
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        control(&mut client, worker::WorkerFrame { body: None })
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM worker_sessions WHERE node_id=$1")
        .bind(&d.node)
        .fetch_one(d.store.pool())
        .await
        .expect("count");
    assert_eq!(count, 0);
    let (sender, mut stream) = control(&mut client, hello(&d.node)).await.expect("control");
    let w = welcome(&mut stream).await;
    sqlx::query("UPDATE worker_sessions SET expires_at=clock_timestamp()-interval '1 second' WHERE node_id=$1").bind(&d.node).execute(d.store.pool()).await.expect("expire");
    assert!(heartbeat(&sender, &mut stream, &d.node, &w).await.is_err());
    drop(sender);
    drop(stream);
    let (sender, mut stream) = control(&mut client, hello(&d.node))
        .await
        .expect("new handshake");
    let next = welcome(&mut stream).await;
    assert!(next.connection_epoch > w.connection_epoch);
    drop(sender);
    drop(stream);
    server.stop().await;
}

#[tokio::test]
async fn database_authority_outage_does_not_use_cached_connection_rights() {
    let d = device("client-h").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, hello(&d.node)).await.expect("control");
    let w = welcome(&mut stream).await;
    // Actual SQLx pool is closed; no mocked success/error response in the Gateway.
    d.store.pool().close().await;
    assert_eq!(
        heartbeat(&sender, &mut stream, &d.node, &w)
            .await
            .expect_err("authority unavailable")
            .code(),
        tonic::Code::Unavailable
    );
    assert_eq!(
        control(&mut client, hello(&d.node))
            .await
            .expect_err("no new grant")
            .code(),
        tonic::Code::Unavailable
    );
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn simultaneous_gateways_only_accept_heartbeat_for_latest_epoch() {
    let d = device("client-i").await;
    let a = Running::start(&d.store).await;
    let b = Running::start(&d.store).await;
    let mut ca = a.client(Some(&d.certificate)).await.expect("a");
    let mut cb = b.client(Some(&d.certificate)).await.expect("b");
    let (left, right) = tokio::join!(
        control(&mut ca, hello(&d.node)),
        control(&mut cb, hello(&d.node))
    );
    let (sa, mut ra) = left.expect("left");
    let (sb, mut rb) = right.expect("right");
    let wa = welcome(&mut ra).await;
    let wb = welcome(&mut rb).await;
    assert_ne!(wa.connection_epoch, wb.connection_epoch);
    let (left, right) = tokio::join!(
        heartbeat(&sa, &mut ra, &d.node, &wa),
        heartbeat(&sb, &mut rb, &d.node, &wb)
    );
    assert_eq!(left.is_ok(), wa.connection_epoch > wb.connection_epoch);
    assert_eq!(right.is_ok(), wb.connection_epoch > wa.connection_epoch);
    drop(sa);
    drop(sb);
    drop(ra);
    drop(rb);
    a.stop().await;
    b.stop().await;
}
#[tokio::test]
async fn organization_membership_is_current_authority_not_certificate_possession() {
    let d = device("client-j").await;
    let organization = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&organization)
        .execute(d.store.pool())
        .await
        .expect("org owner");
    sqlx::query(
        "INSERT INTO organizations(organization_id,name) VALUES($1,'Worker test organization')",
    )
    .bind(&organization)
    .execute(d.store.pool())
    .await
    .expect("org");
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'billing')")
        .bind(&organization)
        .bind(&d.user)
        .execute(d.store.pool())
        .await
        .expect("member");
    sqlx::query("UPDATE nodes SET owner_id=$1 WHERE node_id=$2")
        .bind(&organization)
        .bind(&d.node)
        .execute(d.store.pool())
        .await
        .expect("node org fixture");
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    assert!(control(&mut client, hello(&d.node)).await.is_err());
    sqlx::query("UPDATE memberships SET role='admin' WHERE organization_id=$1 AND user_id=$2")
        .bind(&organization)
        .bind(&d.user)
        .execute(d.store.pool())
        .await
        .expect("admin");
    let (sender, mut stream) = control(&mut client, hello(&d.node))
        .await
        .expect("admin control");
    let w = welcome(&mut stream).await;
    sqlx::query("UPDATE memberships SET active=false WHERE organization_id=$1 AND user_id=$2")
        .bind(&organization)
        .bind(&d.user)
        .execute(d.store.pool())
        .await
        .expect("remove");
    assert!(heartbeat(&sender, &mut stream, &d.node, &w).await.is_err());
    drop(sender);
    drop(stream);
    server.stop().await;
}

fn worker_config(server: &Running, device: &Device) -> orange_worker::control::ControlConfig {
    orange_worker::control::ControlConfig {
        endpoint: format!("https://127.0.0.1:{}/", server.port),
        server_name: "localhost".into(),
        server_ca_pem: file("ca.pem"),
        certificate_pem: file(&format!("{}.pem", device.certificate)),
        private_key_pem: file(&format!("{}.key", device.certificate)),
        node_id: device.node.clone(),
        capabilities_digest: "a".repeat(64),
    }
}

#[tokio::test]
async fn duplicate_node_receipt_has_one_audit_effect_and_cannot_change_receipt_identity() {
    let d = device("client-s").await;
    let (command, action) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let mut unsupported = node_hello(&d.node);
    if let Some(worker_frame::Body::Hello(value)) = &mut unsupported.body {
        value.node_control_version = 99;
    }
    assert_eq!(
        control(&mut client, unsupported)
            .await
            .expect_err("unsupported optional capability")
            .code(),
        tonic::Code::FailedPrecondition
    );
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("control");
    let welcome = welcome(&mut stream).await;
    let delivered = node_frame(&mut stream).await;
    let original = receipt(&delivered);
    sender.send(original.clone()).await.expect("receipt");
    sender.send(original).await.expect("same receipt retry");
    heartbeat(&sender, &mut stream, &d.node, &welcome)
        .await
        .expect("ordered after both receipt frames");
    assert_eq!(state(&d, &command).await, "received");
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM worker_node_command_events WHERE command_id=$1 AND kind='command.received'").bind(&command).fetch_one(d.store.pool()).await.expect("single audit");
    assert_eq!(count, 1);
    let version: String =
        sqlx::query_scalar("SELECT document->>'version' FROM admin_actions WHERE action_id=$1")
            .bind(&action)
            .fetch_one(d.store.pool())
            .await
            .expect("action unchanged");
    assert_eq!(version, "v2");
    sender
        .send(receipt(&delivered))
        .await
        .expect("different receipt ID fixture");
    assert_eq!(
        stream
            .message()
            .await
            .expect_err("cannot replace durable receipt")
            .code(),
        tonic::Code::InvalidArgument
    );
    assert_eq!(state(&d, &command).await, "received");
    drop(sender);
    drop(stream);
    server.stop().await;
}

// Accepted Admin intent fixture. The Admin HTTP/MFA producer is independently
// verified in admin_actions and the real Admin browser suite, not replaced here.
async fn enqueue_stop(d: &Device, generation: i64, kind: &str) -> (String, String) {
    use serde_json::json;
    let command = Uuid::new_v4().to_string();
    let action = Uuid::new_v4().to_string();
    let principal = Uuid::new_v4().to_string();
    let mut tx = d.store.pool().begin().await.expect("tx");
    let version: i64 = sqlx::query_scalar("SELECT version FROM nodes WHERE node_id=$1 FOR UPDATE")
        .bind(&d.node)
        .fetch_one(&mut *tx)
        .await
        .expect("version");
    let now: i64 =
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .fetch_one(&mut *tx)
            .await
            .expect("time");
    let state = match kind {
        "node.drain" => "draining",
        "node.quarantine" => "quarantined",
        _ => panic!("fixture kind"),
    };
    sqlx::query(
        "UPDATE nodes SET state=$2,policy_generation=$3,version=version+1 WHERE node_id=$1",
    )
    .bind(&d.node)
    .bind(state)
    .bind(generation)
    .execute(&mut *tx)
    .await
    .expect("accepted policy fixture");
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,'node-command-fixture',$1)").bind(&principal).execute(&mut *tx).await.expect("fixture principal");
    let mut payload = json!({"node_id":d.node});
    if kind == "node.quarantine" {
        payload["reason_code"] = json!("test_maintenance");
    }
    let request = json!({"scope":{"environment":"dev","kind":"resource","scope_id":d.node},"target_version":format!("v{version}"),"ticket_id":"test-node-command","reason":"Accepted intent fixture for real transport validation","action_type":kind,"payload":payload});
    let doc = json!({"action_id":action,"requested_by":principal,"request":request,"payload_digest":orange_protocol::request_digest("admin-action-payload-v1",&request).expect("digest"),"policy_version":"admin-node-policy-v1","approval_policy":"single_operator","required_approvals":0,"approvals":[],"state":"executing","version":"v2","created_at":orange_control_plane::admin::timestamp(now).expect("timestamp"),"expires_at":orange_control_plane::admin::timestamp(now+900).expect("timestamp"),"business_ref":command});
    sqlx::query("INSERT INTO admin_actions(action_id,environment,scope_kind,scope_id,action_type,document) VALUES($1,'dev','resource',$2,$3,$4)").bind(&action).bind(&d.node).bind(kind).bind(doc).execute(&mut *tx).await.expect("action projection");
    sqlx::query("INSERT INTO admin_action_control(action_id,expires_at,revision) VALUES($1,$2,2)")
        .bind(&action)
        .bind(now + 900)
        .execute(&mut *tx)
        .await
        .expect("managed action");
    sqlx::query("INSERT INTO admin_node_commands(command_id,action_id,node_id,command_kind,target_version,state,created_at,policy_generation) VALUES($1,$2,$3,$4,$5,'queued',$6,$7)").bind(&command).bind(&action).bind(&d.node).bind(kind).bind(version).bind(now).bind(generation).execute(&mut *tx).await.expect("command");
    tx.commit().await.expect("commit fixture");
    (command, action)
}
fn node_hello(node: &str) -> worker::WorkerFrame {
    let mut frame = hello(node);
    if let Some(worker_frame::Body::Hello(value)) = &mut frame.body {
        value.node_control_version = 1;
    }
    frame
}
async fn node_frame(stream: &mut Streaming<worker::ServerFrame>) -> worker::NodeCommand {
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("node command deadline")
        .expect("transport")
        .expect("frame");
    match frame.body {
        Some(server_frame::Body::NodeCommand(command)) => command,
        other => panic!("unexpected frame {other:?}"),
    }
}
fn receipt(command: &worker::NodeCommand) -> worker::WorkerFrame {
    worker::WorkerFrame {
        body: Some(worker_frame::Body::NodeReceipt(worker::NodeReceipt {
            command_id: command.command_id.clone(),
            node_id: command.node_id.clone(),
            policy_generation: command.policy_generation,
            payload_digest: command.payload_digest.clone(),
            receipt_id: Uuid::new_v4().to_string(),
            session_id: command.session_id.clone(),
            connection_epoch: command.connection_epoch,
        })),
    }
}
async fn state(d: &Device, command: &str) -> String {
    sqlx::query_scalar("SELECT state FROM admin_node_commands WHERE command_id=$1")
        .bind(command)
        .fetch_one(d.store.pool())
        .await
        .expect("state")
}
async fn wait_state(d: &Device, command: &str, expected: &str) {
    tokio::time::timeout(Duration::from_secs(6), async {
        while state(d, command).await != expected {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("bounded state convergence");
}
fn journal(d: &Device) -> (PathBuf, orange_worker::node_journal::NodeJournal) {
    let path =
        PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("private test directory"))
            .join(format!("{}.sqlite", Uuid::new_v4()));
    let journal = orange_worker::node_journal::NodeJournal::open(&path, d.node.clone())
        .expect("private receipt journal");
    (path, journal)
}
async fn collect_intent(
    client: &mut orange_worker::control::ControlConnection,
    journal: &mut orange_worker::node_journal::NodeJournal,
) -> orange_worker::node_journal::ReceivedNodeIntent {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let mut received = client
                .heartbeat_node_control("node-snapshot", journal)
                .await
                .expect("real durable receipt client");
            if !received.is_empty() {
                return received.remove(0);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("bounded command arrival")
}

#[tokio::test]
async fn node_commands_require_negotiation_and_real_local_receipt_is_not_cleanup() {
    use orange_worker::control::ControlConnection;
    let d = device("client-m").await;
    let (command, action) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let mut legacy = ControlConnection::connect(worker_config(&server, &d))
        .await
        .expect("legacy control");
    legacy
        .heartbeat("legacy-snapshot")
        .await
        .expect("only heartbeat, no unsolicited command");
    assert_eq!(state(&d, &command).await, "queued");
    legacy.close();
    let (path, mut local) = journal(&d);
    let mut client =
        ControlConnection::connect_node_control(worker_config(&server, &d), &mut local)
            .await
            .expect("negotiated node control");
    let received = collect_intent(&mut client, &mut local).await;
    assert_eq!(received.intent.command_id, command);
    assert_eq!(received.intent.policy_generation, "1");
    assert!(matches!(
        client
            .authorize_node_stop("receipt-only-version", &mut local, &command)
            .await,
        Err(orange_worker::control::ControlError::Unsupported)
    ));
    wait_state(&d, &command, "received").await;
    let status: String =
        sqlx::query_scalar("SELECT document->>'state' FROM admin_actions WHERE action_id=$1")
            .bind(&action)
            .fetch_one(d.store.pool())
            .await
            .expect("action");
    assert_eq!(status, "executing");
    let node_state: String = sqlx::query_scalar("SELECT state FROM nodes WHERE node_id=$1")
        .bind(&d.node)
        .fetch_one(d.store.pool())
        .await
        .expect("node");
    assert_eq!(node_state, "draining");
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM worker_node_command_events WHERE command_id=$1 AND kind='command.received'").bind(&command).fetch_one(d.store.pool()).await.expect("audit");
    assert_eq!(count, 1);
    assert!(
        sqlx::query("DELETE FROM worker_node_command_events WHERE command_id=$1")
            .bind(&command)
            .execute(d.store.pool())
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE admin_node_commands SET policy_generation=policy_generation+1 WHERE command_id=$1").bind(&command).execute(d.store.pool()).await.is_err());
    client.close();
    drop(local);
    let restored = orange_worker::node_journal::NodeJournal::open(&path, d.node.clone())
        .expect("restart receipt store");
    assert_eq!(
        restored.received(&command).expect("durable lookup"),
        Some(received)
    );
    drop(restored);
    server.stop().await;
}
#[tokio::test]
async fn lost_node_receipt_becomes_unknown_and_is_not_replayed_after_reconnect() {
    let d = device("client-n").await;
    let (command, action) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("control");
    let old = welcome(&mut stream).await;
    assert_eq!(old.node_control_version, 1);
    let delivered = node_frame(&mut stream).await;
    assert_eq!(delivered.command_id, command);
    drop(sender);
    drop(stream);
    wait_state(&d, &command, "unknown").await;
    let status: String =
        sqlx::query_scalar("SELECT document->>'state' FROM admin_actions WHERE action_id=$1")
            .bind(&action)
            .fetch_one(d.store.pool())
            .await
            .expect("action");
    assert_eq!(status, "unknown");
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("new control");
    let new = welcome(&mut stream).await;
    assert!(new.connection_epoch > old.connection_epoch);
    heartbeat(&sender, &mut stream, &d.node, &new)
        .await
        .expect("unknown command not reissued");
    sender
        .send(receipt(&delivered))
        .await
        .expect("late old-session receipt fixture");
    assert_eq!(
        stream.message().await.expect_err("stale receipt").code(),
        tonic::Code::InvalidArgument
    );
    assert_eq!(state(&d, &command).await, "unknown");
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn mismatched_node_receipt_digest_cannot_acknowledge_a_different_intent() {
    let d = device("client-o").await;
    let (command, _) = enqueue_stop(&d, 1, "node.quarantine").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let delivered = node_frame(&mut stream).await;
    let mut wrong = receipt(&delivered);
    if let Some(worker_frame::Body::NodeReceipt(value)) = &mut wrong.body {
        value.payload_digest = "0".repeat(64);
    }
    sender.send(wrong).await.expect("mismatched fixture");
    assert_eq!(
        stream.message().await.expect_err("rejected").code(),
        tonic::Code::FailedPrecondition
    );
    wait_state(&d, &command, "unknown").await;
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn node_receipt_deadline_does_not_make_uncertain_work_successful() {
    let d = device("client-p").await;
    let (command, _) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let delivered = node_frame(&mut stream).await;
    sqlx::query("UPDATE admin_node_commands SET dispatched_at=clock_timestamp()-interval '20 seconds' WHERE command_id=$1").bind(&command).execute(d.store.pool()).await.expect("explicit accelerated timeout fixture");
    wait_state(&d, &command, "unknown").await;
    sender
        .send(receipt(&delivered))
        .await
        .expect("late receipt");
    assert_eq!(
        stream
            .message()
            .await
            .expect_err("late result cannot reverse unknown")
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(state(&d, &command).await, "unknown");
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn newer_node_policy_supersedes_only_unsent_work_and_preserves_generation() {
    use orange_worker::control::ControlConnection;
    let d = device("client-q").await;
    let (old, old_action) = enqueue_stop(&d, 1, "node.drain").await;
    let (current, _) = enqueue_stop(&d, 2, "node.quarantine").await;
    let server = Running::start(&d.store).await;
    let (_, mut local) = journal(&d);
    let mut client =
        ControlConnection::connect_node_control(worker_config(&server, &d), &mut local)
            .await
            .expect("node control");
    let received = collect_intent(&mut client, &mut local).await;
    assert_eq!(received.intent.command_id, current);
    assert_eq!(received.intent.policy_generation, "2");
    wait_state(&d, &current, "received").await;
    assert_eq!(state(&d, &old).await, "failed");
    let status: String =
        sqlx::query_scalar("SELECT document->>'state' FROM admin_actions WHERE action_id=$1")
            .bind(&old_action)
            .fetch_one(d.store.pool())
            .await
            .expect("superseded");
    assert_eq!(status, "failed");
    assert!(local.received(&old).expect("no stale delivery").is_none());
    client.close();
    server.stop().await;
}
#[tokio::test]
async fn credential_revocation_between_node_dispatch_and_receipt_has_no_ack_effect() {
    let d = device("client-r").await;
    let (command, _) = enqueue_stop(&d, 1, "node.drain").await;
    let server = Running::start(&d.store).await;
    let mut client = server.client(Some(&d.certificate)).await.expect("TLS");
    let (sender, mut stream) = control(&mut client, node_hello(&d.node))
        .await
        .expect("control");
    welcome(&mut stream).await;
    let delivered = node_frame(&mut stream).await;
    sqlx::query("UPDATE worker_credentials SET active=false WHERE certificate_sha256=$1")
        .bind(&d.fingerprint)
        .execute(d.store.pool())
        .await
        .expect("revoke");
    sender
        .send(receipt(&delivered))
        .await
        .expect("revoked receipt fixture");
    assert_eq!(
        stream
            .message()
            .await
            .expect_err("live authorization required")
            .code(),
        tonic::Code::Unauthenticated
    );
    wait_state(&d, &command, "unknown").await;
    drop(sender);
    drop(stream);
    server.stop().await;
}
#[tokio::test]
async fn real_worker_client_rejects_plaintext_and_loses_authority_after_revoke() {
    use orange_worker::control::{ControlConnection, ControlError};
    let d = device("client-k").await;
    let server = Running::start(&d.store).await;
    let mut plaintext = worker_config(&server, &d);
    plaintext.endpoint = format!("http://127.0.0.1:{}/", server.port);
    assert!(matches!(
        ControlConnection::connect(plaintext).await,
        Err(ControlError::Invalid)
    ));
    let mut client = ControlConnection::connect(worker_config(&server, &d))
        .await
        .expect("real Worker client");
    assert_eq!(client.identity().node_id, d.node);
    assert!(client.is_current_locally());
    client
        .heartbeat("worker-observation-1")
        .await
        .expect("heartbeat");
    sqlx::query("UPDATE worker_credentials SET active=false WHERE certificate_sha256=$1")
        .bind(&d.fingerprint)
        .execute(d.store.pool())
        .await
        .expect("revoke");
    assert!(matches!(
        client.heartbeat("worker-observation-2").await,
        Err(ControlError::Unauthorized)
    ));
    assert!(!client.is_current_locally());
    assert!(matches!(
        client.heartbeat("worker-observation-3").await,
        Err(ControlError::Stale)
    ));
    client.close();
    server.stop().await;
}
#[tokio::test]
async fn real_worker_client_old_epoch_and_dropped_heartbeat_cannot_remain_active() {
    use orange_worker::control::{ControlConnection, ControlError};
    let d = device("client-l").await;
    let a = Running::start(&d.store).await;
    let b = Running::start(&d.store).await;
    let mut old = ControlConnection::connect(worker_config(&a, &d))
        .await
        .expect("old");
    let mut new = ControlConnection::connect(worker_config(&b, &d))
        .await
        .expect("new");
    assert!(new.identity().connection_epoch > old.identity().connection_epoch);
    assert!(matches!(
        old.heartbeat("snapshot").await,
        Err(ControlError::Stale)
    ));
    assert!(!old.is_current_locally());
    old.close();
    new.heartbeat("snapshot").await.expect("new current");
    let mut pending = Box::pin(new.heartbeat("snapshot-next"));
    std::future::poll_fn(|cx| match std::future::Future::poll(pending.as_mut(), cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(_) => panic!("expected real network heartbeat to yield"),
    })
    .await;
    drop(pending);
    assert!(
        !new.is_current_locally(),
        "dropped future must not keep control authority"
    );
    assert!(matches!(
        new.heartbeat("snapshot-again").await,
        Err(ControlError::Stale)
    ));
    new.close();
    a.stop().await;
    b.stop().await;
}
