//! Development platform Host. Its only external process is the fixed local
//! OCI backend; the user input travels on private stdin, never argv or logs.
use orange_protocol::{
    grpc::host::{self, platform_host_pool_client::PlatformHostPoolClient},
    valid_digest, valid_id,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use tokio::sync::watch;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
mod driver;
mod journal;
use driver::{Driver, Session};
use journal::{Entry, Journal, Phase};
const PROFILE: &str = "dev-gvisor-linux-arm64-v1";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub host_instance_id: String,
    pub attestation_digest: String,
    pub ca: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub journal_dir: PathBuf,
    pub driver_root: PathBuf,
    pub archive: PathBuf,
    pub python: PathBuf,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid local Host configuration or journal")]
    Local,
    #[error("OCI backend could not be launched")]
    NeverLaunched,
    #[error("OCI backend or evidence unavailable")]
    Driver,
    #[error("original allocation remains unresolved")]
    Recovery,
    #[error("platform Host transport unavailable")]
    Transport,
    #[error("platform Host control rejected the operation: {0}")]
    Rpc(tonic::Code),
}
impl From<tonic::Status> for Error {
    fn from(e: tonic::Status) -> Self {
        Self::Rpc(e.code())
    }
}
fn hash(v: &Value) -> Result<String, Error> {
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(v).map_err(|_| Error::Local)?,
    )))
}

fn key_bytes(path: &std::path::Path) -> Result<Vec<u8>, Error> {
    use std::io::Read;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut key = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| Error::Local)?;
    let metadata = key.metadata().map_err(|_| Error::Local)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.len() > 65536 {
        return Err(Error::Local);
    }
    let mut bytes = Vec::new();
    key.read_to_end(&mut bytes).map_err(|_| Error::Local)?;
    Ok(bytes)
}
pub struct Node {
    client: PlatformHostPoolClient<Channel>,
    host: String,
    attestation: String,
    journal: Journal,
    driver: Driver,
}
impl Node {
    pub async fn connect(c: Config) -> Result<Self, Error> {
        let endpoint =
            tonic::transport::Endpoint::from_shared(c.endpoint).map_err(|_| Error::Local)?;
        if endpoint.uri().scheme_str() != Some("https")
            || !matches!(
                endpoint.uri().host(),
                Some("localhost" | "127.0.0.1" | "[::1]")
            )
            || !valid_id(&c.host_instance_id)
            || !valid_digest(&c.attestation_digest)
        {
            return Err(Error::Local);
        }
        let tls = ClientTlsConfig::new()
            .domain_name("localhost")
            .ca_certificate(Certificate::from_pem(
                std::fs::read(c.ca).map_err(|_| Error::Local)?,
            ))
            .identity(Identity::from_pem(
                std::fs::read(c.certificate).map_err(|_| Error::Local)?,
                key_bytes(&c.private_key)?,
            ));
        let channel = endpoint
            .tls_config(tls)
            .map_err(|_| Error::Local)?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(7))
            .connect()
            .await
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            client: PlatformHostPoolClient::new(channel),
            host: c.host_instance_id,
            attestation: c.attestation_digest,
            journal: Journal::open(&c.journal_dir)?,
            driver: Driver::new(&c.driver_root, &c.archive, &c.python)?,
        })
    }
    async fn poll(&mut self, accept: bool) -> Result<host::HostPollResponse, Error> {
        Ok(self
            .client
            .poll(host::HostPollRequest {
                host_instance_id: self.host.clone(),
                resource_profile_id: PROFILE.into(),
                attestation_digest: self.attestation.clone(),
                accept_reservations: accept,
                input_protocol_version: 1,
            })
            .await?
            .into_inner())
    }
    pub async fn serve(mut self, mut shutdown: watch::Receiver<bool>) -> Result<(), Error> {
        self.poll(false).await?;
        for e in self.journal.entries()? {
            if e.host != self.host {
                return Err(Error::Local);
            }
            if matches!(e.phase, Phase::Starting | Phase::Ready | Phase::Claimed) {
                let reclaim = self
                    .client
                    .stop_workspace(host::WorkspaceStop {
                        host_instance_id: self.host.clone(),
                        run_id: e.run,
                        workspace_id: e.workspace,
                        allocation_id: e.allocation.ok_or(Error::Recovery)?,
                        allocation_epoch: e.epoch.ok_or(Error::Recovery)?,
                        reason: host::workspace_stop::Reason::LeaseLost as i32,
                    })
                    .await?
                    .into_inner();
                self.recover(reclaim).await?;
            }
        }
        let mut backoff = 1;
        while !*shutdown.borrow() {
            let result = self.step(&mut shutdown).await;
            match result {
                Ok(()) => backoff = 1,
                Err(
                    Error::Rpc(tonic::Code::Unavailable | tonic::Code::DeadlineExceeded)
                    | Error::Transport,
                ) => backoff = (backoff * 2).min(30),
                Err(e) => {
                    let _ = self.poll(false).await;
                    return Err(e);
                }
            }
            tokio::select! {_=shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_secs(backoff))=>{}}
        }
        self.poll(false).await?;
        Ok(())
    }
    pub async fn step(&mut self, shutdown: &mut watch::Receiver<bool>) -> Result<(), Error> {
        let poll = self.poll(true).await?;
        if let Some(reclaim) = poll.reclaim {
            return self.recover(reclaim).await;
        }
        let Some(next) = poll.assignment else {
            return Ok(());
        };
        let mut entry = match self.journal.load(&next.workspace_id)? {
            Some(e)
                if e.run == next.run_id
                    && e.host == self.host
                    && e.expected == next.workspace_version
                    && e.phase == Phase::ClaimIntent =>
            {
                e
            }
            Some(_) => return Err(Error::Recovery),
            None => {
                let e = Entry {
                    host: self.host.clone(),
                    run: next.run_id.clone(),
                    workspace: next.workspace_id.clone(),
                    expected: next.workspace_version,
                    command: uuid::Uuid::new_v4().to_string(),
                    allocation: None,
                    epoch: None,
                    input_sha: None,
                    phase: Phase::ClaimIntent,
                    cleanup_digest: None,
                    readiness_digest: None,
                };
                self.journal.save(&e)?;
                e
            }
        };
        let claim = self
            .client
            .claim_workspace(host::ClaimWorkspaceRequest {
                host_instance_id: self.host.clone(),
                run_id: entry.run.clone(),
                workspace_id: entry.workspace.clone(),
                expected_workspace_version: entry.expected,
                command_id: entry.command.clone(),
            })
            .await;
        let lease = match claim {
            Ok(response) => response.into_inner(),
            Err(status)
                if matches!(
                    status.code(),
                    tonic::Code::FailedPrecondition | tonic::Code::NotFound
                ) =>
            {
                return Ok(());
            }
            Err(status) => return Err(status.into()),
        };
        if lease.host_instance_id != self.host
            || lease.workspace_id != entry.workspace
            || lease.run_id != entry.run
            || lease.epoch == 0
            || uuid::Uuid::parse_str(&lease.allocation_id).is_err()
        {
            return Err(Error::Recovery);
        }
        entry.allocation = Some(lease.allocation_id.clone());
        entry.epoch = Some(lease.epoch);
        entry.phase = Phase::Claimed;
        self.journal.save(&entry)?;
        let max = next.workspace_expires_at.ok_or(Error::Local)?.seconds;
        let result = self
            .run_workspace(&mut entry, lease.clone(), max, shutdown)
            .await;
        let reason = if *shutdown.borrow() {
            host::workspace_stop::Reason::HostShutdown
        } else if result.is_err() {
            host::workspace_stop::Reason::PreparationFailed
        } else {
            host::workspace_stop::Reason::LeaseLost
        };
        let reclaim = self
            .client
            .stop_workspace(host::WorkspaceStop {
                host_instance_id: self.host.clone(),
                run_id: entry.run.clone(),
                workspace_id: entry.workspace.clone(),
                allocation_id: lease.allocation_id,
                allocation_epoch: lease.epoch,
                reason: reason as i32,
            })
            .await?
            .into_inner();
        self.recover(reclaim).await?;
        match result {
            Err(Error::Rpc(tonic::Code::FailedPrecondition | tonic::Code::NotFound))
            | Err(Error::NeverLaunched | Error::Driver) => {
                eprintln!(
                    "Allocation closed after preparation failed; original cleanup acknowledged"
                );
                Ok(())
            }
            other => other,
        }
    }
    async fn run_workspace(
        &mut self,
        entry: &mut Entry,
        lease: host::WorkspaceLease,
        max: i64,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), Error> {
        let input = self
            .client
            .fetch_workspace_input(lease.clone())
            .await?
            .into_inner();
        if input.allocation_id != lease.allocation_id
            || input.content.len() > 262144
            || input.content.is_empty()
            || hex::encode(Sha256::digest(&input.content)) != input.sha256
            || !valid_id(&input.receipt_id)
        {
            return Err(Error::Recovery);
        }
        entry.input_sha = Some(input.sha256.clone());
        let registered = self.driver.reserve(&lease, &input.sha256, max).await?;
        Self::identity(entry, &registered)?;
        if registered["phase"] != "registered" {
            return Err(Error::Recovery);
        }
        entry.phase = Phase::Starting;
        self.journal.save(entry)?;
        let mut session = match self.driver.start(&lease, &input, max).await {
            Err(Error::NeverLaunched) => {
                entry.phase = Phase::Claimed;
                self.journal.save(entry)?;
                return Err(Error::NeverLaunched);
            }
            other => other?,
        };
        let result = self
            .hold(entry, lease, &input.receipt_id, &mut session, shutdown)
            .await;
        session.stop().await;
        // The durable guest receipt, including cleanup after a broken controller
        // connection, is independently recovered before the pool slot is released.
        let exit = session.exit().await;
        result.and(exit)
    }
    async fn hold(
        &mut self,
        e: &mut Entry,
        mut lease: host::WorkspaceLease,
        input_receipt: &str,
        session: &mut Session,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), Error> {
        let mut tick = tokio::time::interval(Duration::from_secs(4));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _=shutdown.changed()=>return Ok(()),
                line=session.next()=>{
                    let record=line?;
                    Self::identity(e,&record)?;
                    if record["phase"]=="cleaned"{return Ok(());}
                    if record["phase"]!="ready"||e.phase!=Phase::Starting||record["input_staged"]!=true||record["workload_uid"]!=65534||
                        record["cgroup"]!=json!({"cpu.max":"50000 100000","memory.max":"134217728","pids.max":"32"}){return Err(Error::Recovery);}
                    let readiness=hash(&record)?;
                    let response=self.client.confirm_ready(host::WorkspaceReady{lease:Some(lease.clone()),input_receipt_id:input_receipt.to_owned(),
                        input_sha256:e.input_sha.clone().ok_or(Error::Local)?,sandbox_id:format!("oa-ws-{}",lease.allocation_id),attestation_digest:self.attestation.clone(),
                        materialization_digest:readiness.clone()}).await?.into_inner();
                    if response.allocation_id!=lease.allocation_id || response.workspace_id!=e.workspace || response.sandbox_id!=record["sandbox_id"]{
                        return Err(Error::Recovery);
                    }
                    e.readiness_digest=Some(readiness);e.phase=Phase::Ready;self.journal.save(e)?;
                }
                _=tick.tick()=>{
                    let state=self.poll(false).await?;
                    if let Some(reclaim)=state.reclaim{
                        if reclaim.allocation_id!=lease.allocation_id||reclaim.claim_command_id!=e.command{return Err(Error::Recovery);}
                        return Ok(());
                    }
                    let fresh=self.client.renew_workspace(lease.clone()).await?.into_inner();
                    if fresh.host_instance_id!=e.host||fresh.allocation_id!=lease.allocation_id||fresh.epoch!=lease.epoch||
                        fresh.run_id!=e.run||fresh.workspace_id!=e.workspace{return Err(Error::Recovery);}
                    session.renew(fresh.lease_until.ok_or(Error::Local)?.seconds).await?;lease=fresh;
                }
            }
        }
    }
    fn identity(e: &Entry, value: &Value) -> Result<(), Error> {
        let alloc = e.allocation.as_deref().ok_or(Error::Recovery)?;
        if value["allocation_id"] != alloc
            || value["run_id"] != e.run
            || value["workspace_id"] != e.workspace
            || value["host_instance_id"] != e.host
            || value["epoch"].as_u64() != e.epoch
            || value["input_sha256"].as_str() != e.input_sha.as_deref()
            || value["sandbox_id"] != format!("oa-ws-{alloc}")
        {
            return Err(Error::Recovery);
        }
        Ok(())
    }
    async fn recover(&mut self, reclaim: host::WorkspaceReclaim) -> Result<(), Error> {
        let mut entry = self
            .journal
            .load(&reclaim.workspace_id)?
            .ok_or(Error::Recovery)?;
        if entry.host != self.host
            || entry.run != reclaim.run_id
            || entry.command != reclaim.claim_command_id
            || entry
                .allocation
                .as_ref()
                .is_some_and(|a| a != &reclaim.allocation_id)
            || entry.epoch.is_some_and(|n| n != reclaim.allocation_epoch)
        {
            return Err(Error::Recovery);
        }
        let proof = match entry.phase {
            Phase::ClaimIntent | Phase::Claimed => hash(
                &json!({"never_started":true,"host":entry.host,"run":entry.run,"workspace":entry.workspace,
                "command":entry.command,"allocation":reclaim.allocation_id,"epoch":reclaim.allocation_epoch}),
            )?,
            Phase::Starting | Phase::Ready => {
                let mut attempt = 0;
                let until = tokio::time::Instant::now() + Duration::from_secs(45);
                let value = loop {
                    match self.driver.recover(&reclaim.allocation_id).await {
                        Ok(v)
                            if v["phase"] == "supervisor_still_owns_allocation"
                                && v["allocation_id"] == reclaim.allocation_id =>
                        {
                            if tokio::time::Instant::now() >= until {
                                return Err(Error::Recovery);
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        Ok(v) => break v,
                        Err(Error::Driver) if attempt < 2 => {
                            attempt += 1;
                            tokio::time::sleep(Duration::from_secs(attempt)).await;
                        }
                        Err(e) => return Err(e),
                    }
                };
                Self::identity(&entry, &value)?;
                if value["phase"] != "cleaned"
                    || value["sandbox_absent"] != true
                    || value["files_absent"] != true
                    || value["cgroup_absent"] != true
                {
                    return Err(Error::Recovery);
                }
                hash(&value)?
            }
            Phase::Cleaned => entry.cleanup_digest.clone().ok_or(Error::Recovery)?,
        };
        entry.allocation = Some(reclaim.allocation_id.clone());
        entry.epoch = Some(reclaim.allocation_epoch);
        entry.cleanup_digest = Some(proof.clone());
        entry.phase = Phase::Cleaned;
        self.journal.save(&entry)?;
        let receipt = self
            .client
            .confirm_cleanup(host::WorkspaceCleanup {
                host_instance_id: self.host.clone(),
                run_id: entry.run,
                workspace_id: entry.workspace.clone(),
                allocation_id: reclaim.allocation_id,
                allocation_epoch: reclaim.allocation_epoch,
                evidence_digest: proof.clone(),
                sandbox_absent: true,
                cgroup_absent: true,
            })
            .await?
            .into_inner();
        if receipt.workspace_id != entry.workspace
            || receipt.evidence_digest != proof
            || !valid_id(&receipt.receipt_id)
        {
            return Err(Error::Recovery);
        }
        Ok(())
    }
}
