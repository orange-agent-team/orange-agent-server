//! Only a platform-registered leaf certificate, on its separate mTLS listener,
//! may refresh this Host's readiness or discover its existing reservations.
use orange_protocol::{
    grpc::host::{
        self,
        platform_host_pool_server::{PlatformHostPool, PlatformHostPoolServer},
    },
    valid_digest, valid_id,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{future::Future, time::Duration};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Request, Response, Status,
    transport::{
        Certificate, Identity, Server, ServerTlsConfig,
        server::{TcpConnectInfo, TlsConnectInfo},
    },
};

#[path = "cloud_host_leases.rs"]
mod leases;

pub struct HostTls {
    pub server_certificate_pem: Vec<u8>,
    pub server_private_key_pem: Vec<u8>,
    pub platform_host_ca_pem: Vec<u8>,
}
#[derive(Clone)]
struct Pool {
    db: PgPool,
}
fn db_error(_: sqlx::Error) -> Status {
    Status::unavailable("host store unavailable")
}
fn leaf<T>(request: &Request<T>) -> Result<String, Status> {
    let chain = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(TlsConnectInfo::peer_certs)
        .ok_or_else(|| Status::unauthenticated("platform host identity required"))?;
    let leaf = chain
        .first()
        .ok_or_else(|| Status::unauthenticated("platform host identity required"))?;
    Ok(hex::encode(Sha256::digest(leaf.as_ref())))
}
fn stamp(seconds: i64) -> prost_types::Timestamp {
    prost_types::Timestamp { seconds, nanos: 0 }
}
#[tonic::async_trait]
impl PlatformHostPool for Pool {
    async fn poll(
        &self,
        request: Request<host::HostPollRequest>,
    ) -> Result<Response<host::HostPollResponse>, Status> {
        let fingerprint = leaf(&request)?;
        let r = request.into_inner();
        if !valid_id(&r.host_instance_id)
            || !valid_id(&r.resource_profile_id)
            || !valid_digest(&r.attestation_digest)
        {
            return Err(Status::invalid_argument("invalid host announcement"));
        }
        let mut tx = self.db.begin().await.map_err(db_error)?;
        let deadline=sqlx::query_scalar::<_,i64>("UPDATE cloud_pool_hosts
            SET live_until=LEAST(credential_expires_at,date_trunc('second',clock_timestamp())+interval '30 seconds'),accepting=$5
            WHERE host_instance_id=$1 AND certificate_sha256=$2 AND profile_id=$3 AND attestation_digest=$4 AND enabled
              AND credential_expires_at>clock_timestamp()+interval '1 second'
            RETURNING FLOOR(EXTRACT(EPOCH FROM live_until))::bigint")
            .bind(&r.host_instance_id).bind(&fingerprint).bind(r.resource_profile_id).bind(r.attestation_digest).bind(r.accept_reservations)
            .fetch_optional(&mut *tx).await.map_err(db_error)?.ok_or_else(||Status::permission_denied("host admission denied"))?;
        let reclaim=sqlx::query("SELECT w.run_id,w.workspace_id,c.allocation_id,c.epoch FROM cloud_workspaces w
            JOIN cloud_workspace_claims c USING(workspace_id,host_instance_id)
            WHERE w.host_instance_id=$1 AND w.status='draining' ORDER BY w.expires_at,w.workspace_id LIMIT 1")
            .bind(&r.host_instance_id).fetch_optional(&mut *tx).await.map_err(db_error)?;
        let reclaim = if let Some(row) = reclaim {
            Some(host::WorkspaceReclaim {
                run_id: row.try_get("run_id").map_err(db_error)?,
                workspace_id: row.try_get("workspace_id").map_err(db_error)?,
                allocation_id: row.try_get("allocation_id").map_err(db_error)?,
                allocation_epoch: u64::try_from(row.try_get::<i64, _>("epoch").map_err(db_error)?)
                    .map_err(|_| Status::internal("invalid allocation epoch"))?,
            })
        } else {
            None
        };
        let row=sqlx::query("SELECT w.workspace_id,w.version AS workspace_version,r.run_id,r.version,FLOOR(EXTRACT(EPOCH FROM w.expires_at))::bigint AS expires
            FROM cloud_workspaces w JOIN cloud_runs r USING(run_id) JOIN owners o ON o.owner_id=r.owner_id JOIN users u ON u.user_id=r.user_id
            JOIN budget_roots b ON b.root_budget_id=r.root_budget_id AND b.owner_id=r.owner_id
            JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id AND a.owner_id=r.owner_id
            WHERE w.host_instance_id=$1 AND w.status='requested' AND w.epoch=0 AND w.expires_at>clock_timestamp()
              AND r.status='queued' AND o.state='active' AND u.active AND u.personal_owner_id=r.owner_id AND b.state='active' AND a.state='available'
            ORDER BY r.sequence LIMIT 1")
            .bind(r.host_instance_id).fetch_optional(&mut *tx).await.map_err(db_error)?;
        let assignment = if let Some(row) = row {
            Some(host::QueuedRun {
                workspace_version: u64::try_from(
                    row.try_get::<i64, _>("workspace_version")
                        .map_err(db_error)?,
                )
                .map_err(|_| Status::internal("invalid workspace version"))?,

                run_id: row.try_get("run_id").map_err(db_error)?,
                workspace_id: row.try_get("workspace_id").map_err(db_error)?,
                expected_run_version: u64::try_from(
                    row.try_get::<i64, _>("version").map_err(db_error)?,
                )
                .map_err(|_| Status::internal("invalid run version"))?,
                workspace_expires_at: Some(stamp(row.try_get("expires").map_err(db_error)?)),
            })
        } else {
            None
        };
        tx.commit().await.map_err(db_error)?;
        Ok(Response::new(host::HostPollResponse {
            heartbeat_expires_at: Some(stamp(deadline)),
            assignment: if reclaim.is_none() { assignment } else { None },
            reclaim,
        }))
    }
    async fn claim_workspace(
        &self,
        request: Request<host::ClaimWorkspaceRequest>,
    ) -> Result<Response<host::WorkspaceLease>, Status> {
        let cert = leaf(&request)?;
        Ok(Response::new(
            leases::claim(&self.db, &cert, request.into_inner()).await?,
        ))
    }
    async fn renew_workspace(
        &self,
        request: Request<host::WorkspaceLease>,
    ) -> Result<Response<host::WorkspaceLease>, Status> {
        let cert = leaf(&request)?;
        Ok(Response::new(
            leases::renew(&self.db, &cert, request.into_inner()).await?,
        ))
    }
    async fn confirm_cleanup(
        &self,
        request: Request<host::WorkspaceCleanup>,
    ) -> Result<Response<host::CleanupReceipt>, Status> {
        let cert = leaf(&request)?;
        Ok(Response::new(
            leases::cleanup(&self.db, &cert, request.into_inner()).await?,
        ))
    }
}
pub async fn serve(
    listener: TcpListener,
    db: PgPool,
    tls: HostTls,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            tls.server_certificate_pem,
            tls.server_private_key_pem,
        ))
        .client_ca_root(Certificate::from_pem(tls.platform_host_ca_pem));
    Server::builder()
        .tls_config(tls)?
        .timeout(Duration::from_secs(7))
        .concurrency_limit_per_connection(8)
        .max_concurrent_streams(16)
        .add_service(
            PlatformHostPoolServer::new(Pool { db })
                .max_decoding_message_size(4096)
                .max_encoding_message_size(8192),
        )
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop)
        .await
}
