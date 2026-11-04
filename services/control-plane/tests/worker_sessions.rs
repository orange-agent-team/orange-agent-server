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
        let handle = tokio::spawn(gateway::serve(listener, tls, sessions, async {
            let _ = receiver.await;
        }));
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
            .err()
            .expect("unsupported")
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
            .err()
            .expect("no new grant")
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
