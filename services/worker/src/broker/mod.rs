//! Persistent arbitration core for trusted OS/transport adapters. This module
//! does not authenticate Named Pipe peers or certify GPU cleanup by itself.
pub(crate) mod storage;
use orange_protocol::{request_digest, valid_digest, valid_id};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    future::Future,
    path::Path,
    pin::Pin,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("invalid broker command or configuration")]
    Invalid,
    #[error("another broker owns this data directory")]
    Busy,
    #[error("unsupported or nonprivate broker storage")]
    StoragePolicy,
    #[error("unsupported user or stale supervisor registration")]
    Unauthorized,
    #[error("stale or expired resource authority")]
    Stale,
    #[error("request identity/content conflict")]
    Conflict,
    #[error("resource requires reconciliation")]
    RecoveryRequired,
    #[error("remote contribution is disabled")]
    RemoteDisabled,
    #[error("verified matching cloud StartGrant required")]
    CloudGrantRequired,
    #[error("device remains occupied or quarantined")]
    Occupied,
    #[error("cleanup probe unavailable or evidence incomplete")]
    EvidenceRequired,
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedProfile {
    pub id: String,
    pub model_sha256: String,
    pub execution_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    pub owner_identity: String,
    pub devices: Vec<String>,
    pub approved_profiles: Vec<ApprovedProfile>,
}
/// Obtained from an OS authenticated local connection. Not deserializable; PID
/// alone is not a process_identity. No remote caller may construct this directly.
pub struct VerifiedPeer {
    owner: String,
    process: String,
    instance: String,
    nonce: String,
}
impl VerifiedPeer {
    pub fn from_verified_os_identity(
        owner: String,
        process_identity: String,
        instance: String,
        nonce: String,
    ) -> Result<Self, BrokerError> {
        if ![&owner, &process_identity, &instance, &nonce]
            .iter()
            .all(|s| valid_id(s))
            || nonce.len() < 32
            || process_identity.len() < 16
        {
            return Err(BrokerError::Invalid);
        }
        Ok(Self {
            owner,
            process: process_identity,
            instance,
            nonce,
        })
    }
}
/// In-process credential returned only to the authenticated transport adapter.
/// Contains no serializable/public fields or Debug representation.
pub struct Supervisor {
    instance: String,
    epoch: i64,
    credential: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holder {
    Local,
    Remote,
}
impl Holder {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteBinding {
    pub node_id: String,
    pub attempt_id: String,
    pub lease_id: String,
    pub connection_epoch: u64,
    pub fencing_token: u64,
}
impl RemoteBinding {
    fn validate(&self) -> Result<(), BrokerError> {
        if ![&self.node_id, &self.attempt_id, &self.lease_id]
            .iter()
            .all(|s| valid_id(s))
            || self.connection_epoch == 0
            || self.fencing_token == 0
        {
            return Err(BrokerError::Invalid);
        }
        Ok(())
    }
}
/// This value is created by the authenticated cloud-client adapter after lease
/// verification. Duration is the conservative remaining interval measured from
/// monotonic_send, not remote wall time or response receive time.
pub struct VerifiedCloudGrant {
    binding: RemoteBinding,
    stop_at: Instant,
}
impl VerifiedCloudGrant {
    pub fn from_verified_start(
        binding: RemoteBinding,
        monotonic_send: Instant,
        granted: Duration,
    ) -> Result<Self, BrokerError> {
        binding.validate()?;
        if granted <= Duration::from_secs(1)
            || granted > Duration::from_secs(45)
            || monotonic_send > Instant::now()
        {
            return Err(BrokerError::Invalid);
        }
        let stop_at = monotonic_send
            .checked_add(granted - Duration::from_secs(1))
            .ok_or(BrokerError::Invalid)?;
        if stop_at <= Instant::now() {
            return Err(BrokerError::Stale);
        }
        Ok(Self { binding, stop_at })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireRequest {
    pub request_id: String,
    pub device_id: String,
    pub mode: Holder,
    pub profile_id: String,
    pub remote: Option<RemoteBinding>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub grant_id: String,
    pub device_id: String,
    pub broker_epoch: i64,
    pub slot_generation: i64,
    pub supervisor_instance: String,
    pub mode: Holder,
    pub profile_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AcquireResult {
    Reserved { grant: Grant },
    DrainRequired { grant: Grant },
    Busy,
    RecoveryRequired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotView {
    pub device_id: String,
    pub state: String,
    pub broker_epoch: i64,
    pub slot_generation: i64,
    pub grant: Option<Grant>,
    pub local_pending: bool,
    pub reason: Option<String>,
}
/// Challenge is issued by Broker after recording drain/reconcile. Probe adapters
/// must inspect all Orange-managed native/container tasks, not just a reported PID.
#[derive(Debug, Clone)]
pub struct ProbeChallenge {
    pub device_id: String,
    pub broker_epoch: i64,
    pub slot_generation: i64,
    pub prior_grant: Option<Grant>,
    pub nonce: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ProbeObservation {
    pub device_id: String,
    pub challenge_nonce: String,
    pub prior_grant_id: Option<String>,
    pub all_managed_tasks_joined: bool,
    pub all_managed_processes_exited: bool,
    pub device_probe_available: bool,
    pub device_healthy: bool,
    pub evidence_digest: String,
}
pub type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProbeObservation, BrokerError>> + Send + 'a>>;
/// Trusted OS adapter, deliberately separate from remote CleanupReport DTOs.
pub trait ResourceProbe {
    fn inspect<'a>(&'a self, challenge: &'a ProbeChallenge) -> ProbeFuture<'a>;
}
#[derive(Debug, Clone, Copy)]
pub enum Invalidation {
    ResumeFromSleep,
    DeviceReset,
    IdentityChanged,
}
pub struct Broker {
    connection: Connection,
    _lock: File,
    epoch: i64,
    started: Instant,
    config: BrokerConfig,
}
type SlotRow = (String, i64, i64, Option<String>, bool, Option<String>);

fn json<T: Serialize>(value: &T) -> Result<String, BrokerError> {
    serde_json::to_string(value).map_err(|_| BrokerError::Invalid)
}
fn digest<T: Serialize>(kind: &str, value: &T) -> Result<String, BrokerError> {
    request_digest(kind, value).map_err(|_| BrokerError::Invalid)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, BrokerError> {
    serde_json::from_str(value).map_err(|_| BrokerError::Invalid)
}
impl Broker {
    pub fn open(path: &Path, config: BrokerConfig) -> Result<Self, BrokerError> {
        if !valid_id(&config.owner_identity)
            || config.devices.is_empty()
            || config.devices.len() > 64
            || config.approved_profiles.is_empty()
            || config.approved_profiles.len() > 1000
        {
            return Err(BrokerError::Invalid);
        }
        let mut devices = std::collections::BTreeSet::new();
        for device in &config.devices {
            if !valid_id(device) || !devices.insert(device) {
                return Err(BrokerError::Invalid);
            }
        }
        let mut profiles = std::collections::BTreeSet::new();
        for profile in &config.approved_profiles {
            if !valid_id(&profile.id)
                || !valid_digest(&profile.model_sha256)
                || !valid_digest(&profile.execution_digest)
                || !profiles.insert(&profile.id)
            {
                return Err(BrokerError::Invalid);
            }
        }
        let (mut connection, lock) = storage::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(0..=1).contains(&version) {
            return Err(BrokerError::StoragePolicy);
        }
        let config_digest = digest("broker-config-v1", &config)?;
        if version == 0 {
            let count:i64=connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
            if count != 0 {
                return Err(BrokerError::StoragePolicy);
            }
        } else {
            let existing: String = connection.query_row(
                "SELECT config_digest FROM broker_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if existing != config_digest {
                return Err(BrokerError::Conflict);
            }
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            tx.execute_batch(include_str!("schema.sql"))?;
            tx.execute(
                "INSERT INTO broker_meta(singleton,epoch,config_digest,owner) VALUES(1,1,?,?)",
                params![config_digest, config.owner_identity],
            )?;
            for device in &config.devices {
                tx.execute(
                    "INSERT INTO slots(device_id,state,epoch) VALUES(?,'reconciling',1)",
                    [device],
                )?;
            }
        } else {
            let exhausted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM broker_meta WHERE epoch=9223372036854775807 UNION SELECT 1 FROM slots WHERE generation=9223372036854775807)",[],|r|r.get(0))?;
            if exhausted {
                return Err(BrokerError::RecoveryRequired);
            }
            tx.execute(
                "UPDATE broker_meta SET epoch=epoch+1,remote_allowed=0 WHERE singleton=1",
                [],
            )?;
            tx.execute("UPDATE slots SET state='reconciling',epoch=(SELECT epoch FROM broker_meta),generation=generation+1,reason='broker_restart'",[])?;
            tx.execute("UPDATE grants SET state='quarantined' WHERE state IN('reserved','running','draining')",[])?;
        }
        let epoch: i64 = tx.query_row("SELECT epoch FROM broker_meta", [], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO broker_events(kind,details) VALUES('broker.reconciling',?)",
            [epoch.to_string()],
        )?;
        tx.commit()?;
        Ok(Self {
            connection,
            _lock: lock,
            epoch,
            started: Instant::now(),
            config,
        })
    }
    fn ticks(&self) -> Result<i64, BrokerError> {
        i64::try_from(self.started.elapsed().as_millis()).map_err(|_| BrokerError::Invalid)
    }
    pub fn register_supervisor(&mut self, peer: VerifiedPeer) -> Result<Supervisor, BrokerError> {
        if peer.owner != self.config.owner_identity {
            return Err(BrokerError::Unauthorized);
        }
        let old: Option<(String, String, i64)> = self
            .connection
            .query_row(
                "SELECT process_identity,nonce,epoch FROM supervisors WHERE instance_id=?",
                [&peer.instance],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((process, nonce, epoch)) = old
            && (process != peer.process || nonce != peer.nonce || epoch == self.epoch)
        {
            return Err(BrokerError::Conflict);
        }
        let credential = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.connection.execute("INSERT INTO supervisors(instance_id,owner,process_identity,nonce,epoch,credential_digest) VALUES(?,?,?,?,?,?) ON CONFLICT(instance_id) DO UPDATE SET epoch=excluded.epoch,credential_digest=excluded.credential_digest",params![peer.instance,peer.owner,peer.process,peer.nonce,self.epoch,digest("supervisor-credential",&credential)?])?;
        Ok(Supervisor {
            instance: peer.instance,
            epoch: self.epoch,
            credential,
        })
    }
    /// Called by the trusted OS adapter before accepting work after sleep/reset.
    pub fn invalidate(&mut self, reason: Invalidation) -> Result<(), BrokerError> {
        let epoch = self
            .epoch
            .checked_add(1)
            .ok_or(BrokerError::RecoveryRequired)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exhausted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM slots WHERE generation=9223372036854775807)",
            [],
            |r| r.get(0),
        )?;
        if exhausted {
            return Err(BrokerError::RecoveryRequired);
        }
        let reason = match reason {
            Invalidation::ResumeFromSleep => "resume_from_sleep",
            Invalidation::DeviceReset => "device_reset",
            Invalidation::IdentityChanged => "identity_changed",
        };
        tx.execute(
            "UPDATE broker_meta SET epoch=?,remote_allowed=0 WHERE singleton=1",
            [epoch],
        )?;
        tx.execute(
            "UPDATE slots SET state='reconciling',epoch=?,generation=generation+1,reason=?",
            params![epoch, reason],
        )?;
        tx.execute(
            "UPDATE grants SET state='quarantined' WHERE state IN('reserved','running','draining')",
            [],
        )?;
        tx.execute(
            "INSERT INTO broker_events(kind,details) VALUES('broker.invalidated',?)",
            [reason],
        )?;
        tx.commit()?;
        self.epoch = epoch;
        Ok(())
    }
    fn authenticate(&self, supervisor: &Supervisor) -> Result<(), BrokerError> {
        if supervisor.epoch != self.epoch {
            return Err(BrokerError::Unauthorized);
        }
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM supervisors WHERE instance_id=? AND owner=? AND epoch=? AND credential_digest=?)",params![supervisor.instance,self.config.owner_identity,self.epoch,digest("supervisor-credential",&supervisor.credential)?],|r|r.get(0))?;
        if !valid {
            return Err(BrokerError::Unauthorized);
        }
        Ok(())
    }
    /// Trusted contribution policy hook. IPC must gate this on the enrolled user.
    /// Revocation atomically drains every current remote grant, including reserved
    /// grants. It never frees a slot or stops a local Agent grant. The supervisor
    /// must stop its tasks and use release() with actual cleanup evidence.
    pub fn set_remote_allowed(&mut self, allowed: bool) -> Result<(), BrokerError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: bool = tx.query_row(
            "SELECT remote_allowed FROM broker_meta WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if !allowed {
            let grants = {
                let mut statement = tx.prepare("SELECT g.device_id,g.grant_id FROM grants g JOIN slots s ON s.grant_id=g.grant_id WHERE g.epoch=? AND g.mode='remote' AND g.state IN('reserved','running') AND s.state IN('reserved_remote','running_remote')")?;
                statement
                    .query_map([self.epoch], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for (device, grant) in grants {
                tx.execute("UPDATE slots SET state='draining',reason='contribution_revoked' WHERE device_id=? AND grant_id=?", params![device,grant])?;
                tx.execute(
                    "UPDATE grants SET state='draining' WHERE grant_id=?",
                    [&grant],
                )?;
                tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.draining','contribution_revoked')", params![device,grant])?;
            }
        }
        tx.execute(
            "UPDATE broker_meta SET remote_allowed=? WHERE singleton=1",
            [allowed],
        )?;
        if previous != allowed {
            tx.execute(
                "INSERT INTO broker_events(kind,details) VALUES('contribution.policy_changed',?)",
                [if allowed { "enabled" } else { "disabled" }],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Durable stop work for this authenticated supervisor. Reading this list is
    /// not a cleanup acknowledgement; entries remain until a verified release.
    pub fn draining_grants(&self, supervisor: &Supervisor) -> Result<Vec<Grant>, BrokerError> {
        self.authenticate(supervisor)?;
        let ids = {
            let mut statement=self.connection.prepare("SELECT g.grant_id FROM grants g JOIN slots s ON s.grant_id=g.grant_id WHERE g.supervisor=? AND g.epoch=? AND g.state='draining' AND s.state='draining' ORDER BY g.device_id")?;
            statement
                .query_map(params![supervisor.instance, self.epoch], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter().map(|id| self.grant(&id)).collect()
    }
    pub fn slot(&self, device: &str) -> Result<SlotView, BrokerError> {
        let row:Option<SlotRow>=self.connection.query_row("SELECT state,epoch,generation,grant_id,local_pending,reason FROM slots WHERE device_id=?",[device],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let (state, broker_epoch, slot_generation, id, local_pending, reason) =
            row.ok_or(BrokerError::Invalid)?;
        let grant = id.map(|id| self.grant(&id)).transpose()?;
        Ok(SlotView {
            device_id: device.into(),
            state,
            broker_epoch,
            slot_generation,
            grant,
            local_pending,
            reason,
        })
    }
    fn grant(&self, id: &str) -> Result<Grant, BrokerError> {
        let(device_id,broker_epoch,slot_generation,supervisor_instance,mode,profile_id):(String,i64,i64,String,String,String)=self.connection.query_row("SELECT device_id,epoch,generation,supervisor,mode,profile_id FROM grants WHERE grant_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        Ok(Grant {
            grant_id: id.into(),
            device_id,
            broker_epoch,
            slot_generation,
            supervisor_instance,
            mode: match mode.as_str() {
                "local" => Holder::Local,
                "remote" => Holder::Remote,
                _ => return Err(BrokerError::Invalid),
            },
            profile_id,
        })
    }
    fn current(&self, supervisor: &Supervisor, grant: &Grant) -> Result<SlotView, BrokerError> {
        self.authenticate(supervisor)?;
        let slot = self.slot(&grant.device_id)?;
        if grant.broker_epoch != self.epoch
            || grant.supervisor_instance != supervisor.instance
            || slot.grant.as_ref() != Some(grant)
            || slot.broker_epoch != grant.broker_epoch
            || slot.slot_generation != grant.slot_generation
        {
            return Err(BrokerError::Stale);
        }
        Ok(slot)
    }
    pub fn acquire(
        &mut self,
        supervisor: &Supervisor,
        request: &AcquireRequest,
    ) -> Result<AcquireResult, BrokerError> {
        self.authenticate(supervisor)?;
        if !valid_id(&request.request_id)
            || !self
                .config
                .approved_profiles
                .iter()
                .any(|p| p.id == request.profile_id)
            || !self.config.devices.contains(&request.device_id)
        {
            return Err(BrokerError::Invalid);
        }
        match (request.mode, &request.remote) {
            (Holder::Local, None) => {}
            (Holder::Remote, Some(remote)) => remote.validate()?,
            _ => return Err(BrokerError::Invalid),
        }
        let hash = digest("broker-acquire-v1", request)?;
        let previous: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT supervisor,digest,result FROM broker_commands WHERE request_id=?",
                [&request.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((actor, previous_hash, result)) = previous {
            if actor != supervisor.instance || previous_hash != hash {
                return Err(BrokerError::Conflict);
            }
            let result: AcquireResult = decode(&result)?;
            if let AcquireResult::Reserved { grant } = &result {
                let slot = self.current(supervisor, grant)?;
                if !matches!(
                    slot.state.as_str(),
                    "reserved_local" | "reserved_remote" | "running_local" | "running_remote"
                ) {
                    return Err(BrokerError::Stale);
                }
                let expires: i64 = self.connection.query_row(
                    "SELECT expires_ms FROM grants WHERE grant_id=?",
                    [&grant.grant_id],
                    |r| r.get(0),
                )?;
                if expires <= self.ticks()? {
                    return Err(BrokerError::Stale);
                }
            }
            return Ok(result);
        }
        let slot = self.slot(&request.device_id)?;
        let allowed: bool =
            self.connection
                .query_row("SELECT remote_allowed FROM broker_meta", [], |r| r.get(0))?;
        if request.mode == Holder::Remote && (!allowed || slot.local_pending) {
            return Err(BrokerError::RemoteDisabled);
        }
        let time = self.ticks()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut may_reserve = slot.state == "idle";
        let mut result = if matches!(slot.state.as_str(), "quarantined" | "reconciling") {
            AcquireResult::RecoveryRequired
        } else {
            AcquireResult::Busy
        };
        if request.mode == Holder::Local {
            tx.execute("UPDATE broker_meta SET remote_allowed=0", [])?;
            if slot.state == "reserved_remote" {
                let prior = slot.grant.as_ref().ok_or(BrokerError::Invalid)?;
                tx.execute(
                    "UPDATE grants SET state='revoked' WHERE grant_id=? AND state='reserved'",
                    [&prior.grant_id],
                )?;
                tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.preempted_before_start','{}')",params![request.device_id,prior.grant_id])?;
                may_reserve = true;
            } else if slot.state == "running_remote" || slot.state == "draining" {
                let prior = slot.grant.as_ref().ok_or(BrokerError::Invalid)?;
                tx.execute("UPDATE slots SET state='draining',local_pending=1,reason='local_priority' WHERE device_id=?",[&request.device_id])?;
                tx.execute(
                    "UPDATE grants SET state='draining' WHERE grant_id=?",
                    [&prior.grant_id],
                )?;
                result = AcquireResult::DrainRequired {
                    grant: prior.clone(),
                };
            }
        }
        if may_reserve {
            let generation = slot
                .slot_generation
                .checked_add(1)
                .ok_or(BrokerError::Invalid)?;
            let grant = Grant {
                grant_id: format!("grant-{}", uuid::Uuid::new_v4()),
                device_id: request.device_id.clone(),
                broker_epoch: self.epoch,
                slot_generation: generation,
                supervisor_instance: supervisor.instance.clone(),
                mode: request.mode,
                profile_id: request.profile_id.clone(),
            };
            tx.execute("INSERT INTO grants(grant_id,device_id,epoch,generation,supervisor,mode,profile_id,remote_binding,state,expires_ms) VALUES(?,?,?,?,?,?,?,?,'reserved',?)",params![grant.grant_id,grant.device_id,grant.broker_epoch,grant.slot_generation,grant.supervisor_instance,request.mode.as_str(),grant.profile_id,request.remote.as_ref().map(json).transpose()?,time+15000])?;
            tx.execute("UPDATE slots SET state=?,epoch=?,generation=?,grant_id=?,local_pending=0,reason=NULL WHERE device_id=?",params![if request.mode==Holder::Local{"reserved_local"}else{"reserved_remote"},self.epoch,generation,grant.grant_id,grant.device_id])?;
            tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.reserved',?)",params![grant.device_id,grant.grant_id,json(&grant)?])?;
            result = AcquireResult::Reserved { grant };
        }
        tx.execute("INSERT INTO broker_commands(request_id,supervisor,kind,digest,result) VALUES(?,?,'acquire',?,?)",params![request.request_id,supervisor.instance,hash,json(&result)?])?;
        tx.commit()?;
        Ok(result)
    }
    pub fn confirm_start(
        &mut self,
        supervisor: &Supervisor,
        grant: &Grant,
        cloud: Option<&VerifiedCloudGrant>,
    ) -> Result<(), BrokerError> {
        let slot = self.current(supervisor, grant)?;
        if !matches!(slot.state.as_str(), "reserved_local" | "reserved_remote") {
            return Err(BrokerError::Stale);
        }
        let now = self.ticks()?;
        let (expires, binding): (i64, Option<String>) = self.connection.query_row(
            "SELECT expires_ms,remote_binding FROM grants WHERE grant_id=?",
            [&grant.grant_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if expires <= now {
            return Err(BrokerError::Stale);
        }
        let remote_deadline = match (grant.mode, cloud) {
            (Holder::Local, None) => None,
            (Holder::Remote, Some(cloud)) => {
                if binding
                    .as_deref()
                    .map(decode::<RemoteBinding>)
                    .transpose()?
                    .as_ref()
                    != Some(&cloud.binding)
                    || cloud.stop_at <= Instant::now()
                {
                    return Err(BrokerError::CloudGrantRequired);
                }
                Some(
                    now + i64::try_from(
                        cloud
                            .stop_at
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                    )
                    .map_err(|_| BrokerError::Invalid)?,
                )
            }
            _ => return Err(BrokerError::CloudGrantRequired),
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE grants SET state='running',expires_ms=?,remote_deadline_ms=? WHERE grant_id=? AND state='reserved'",params![(now+15000).min(remote_deadline.unwrap_or(i64::MAX)),remote_deadline,grant.grant_id])?;
        tx.execute(
            "UPDATE slots SET state=? WHERE device_id=?",
            params![
                if grant.mode == Holder::Local {
                    "running_local"
                } else {
                    "running_remote"
                },
                grant.device_id
            ],
        )?;
        tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.start_confirmed','{}')",params![grant.device_id,grant.grant_id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn renew(
        &mut self,
        supervisor: &Supervisor,
        grant: &Grant,
        cloud: Option<&VerifiedCloudGrant>,
    ) -> Result<(), BrokerError> {
        let slot = self.current(supervisor, grant)?;
        if !matches!(slot.state.as_str(), "running_local" | "running_remote") {
            return Err(BrokerError::Stale);
        }
        let now = self.ticks()?;
        let (expires, old_remote, binding): (i64, Option<i64>, Option<String>) =
            self.connection.query_row(
                "SELECT expires_ms,remote_deadline_ms,remote_binding FROM grants WHERE grant_id=?",
                [&grant.grant_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        if expires <= now || old_remote.is_some_and(|d| d <= now) {
            return Err(BrokerError::Stale);
        }
        let deadline = match (grant.mode, cloud) {
            (Holder::Local, None) => None,
            (Holder::Remote, None) => old_remote,
            (Holder::Remote, Some(cloud)) => {
                if binding
                    .as_deref()
                    .map(decode::<RemoteBinding>)
                    .transpose()?
                    .as_ref()
                    != Some(&cloud.binding)
                    || cloud.stop_at <= Instant::now()
                {
                    return Err(BrokerError::CloudGrantRequired);
                }
                Some(
                    now + i64::try_from(
                        cloud
                            .stop_at
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                    )
                    .map_err(|_| BrokerError::Invalid)?,
                )
            }
            _ => return Err(BrokerError::Invalid),
        };
        self.connection.execute(
            "UPDATE grants SET expires_ms=?,remote_deadline_ms=? WHERE grant_id=?",
            params![
                (now + 15000).min(deadline.unwrap_or(i64::MAX)),
                deadline,
                grant.grant_id
            ],
        )?;
        Ok(())
    }
    pub fn begin_drain(
        &mut self,
        supervisor: &Supervisor,
        grant: &Grant,
    ) -> Result<(), BrokerError> {
        let slot = self.current(supervisor, grant)?;
        if matches!(slot.state.as_str(), "idle" | "reconciling" | "quarantined") {
            return Err(BrokerError::Stale);
        }
        self.drain(grant, "requested")
    }
    fn drain(&mut self, grant: &Grant, reason: &str) -> Result<(), BrokerError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE slots SET state='draining',reason=? WHERE device_id=?",
            params![reason, grant.device_id],
        )?;
        tx.execute(
            "UPDATE grants SET state='draining' WHERE grant_id=?",
            [&grant.grant_id],
        )?;
        tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.draining',?)",params![grant.device_id,grant.grant_id,reason])?;
        tx.commit()?;
        Ok(())
    }
    /// Expiration stops authorization; it never makes a device idle.
    pub fn expire(&mut self) -> Result<Vec<Grant>, BrokerError> {
        let now = self.ticks()?;
        let ids = {
            let mut s=self.connection.prepare("SELECT grant_id FROM grants WHERE epoch=? AND state IN('reserved','running') AND expires_ms<=?")?;
            s.query_map(params![self.epoch, now], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut expired = Vec::new();
        for id in ids {
            let grant = self.grant(&id)?;
            self.drain(&grant, "lease_expired")?;
            expired.push(grant);
        }
        Ok(expired)
    }
    pub async fn release<P: ResourceProbe>(
        &mut self,
        supervisor: &Supervisor,
        grant: &Grant,
        probe: &P,
    ) -> Result<(), BrokerError> {
        let slot = self.current(supervisor, grant)?;
        if slot.state != "draining" {
            return Err(BrokerError::Stale);
        }
        self.inspect_and_reconcile(&grant.device_id, probe).await
    }
    /// Trusted startup/driver-reset recovery path. Remote reports alone never
    /// suffice: this method invokes a local OS probe before changing occupancy.
    pub async fn reconcile<P: ResourceProbe>(
        &mut self,
        device: &str,
        probe: &P,
    ) -> Result<(), BrokerError> {
        let slot = self.slot(device)?;
        if !matches!(slot.state.as_str(), "reconciling" | "quarantined") {
            return Err(BrokerError::Stale);
        }
        self.inspect_and_reconcile(device, probe).await
    }
    async fn inspect_and_reconcile<P: ResourceProbe>(
        &mut self,
        device: &str,
        probe: &P,
    ) -> Result<(), BrokerError> {
        let slot = self.slot(device)?;
        let challenge = ProbeChallenge {
            device_id: device.into(),
            broker_epoch: self.epoch,
            slot_generation: slot.slot_generation,
            prior_grant: slot.grant.clone(),
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        };
        // Persist quarantine before starting an interruptible probe. Dropping the
        // future cannot leave an idle slot or a completed release record.
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE slots SET state='quarantined',reason='probe_pending' WHERE device_id=?",
            [device],
        )?;
        if let Some(grant) = &slot.grant {
            tx.execute(
                "UPDATE grants SET state='quarantined' WHERE grant_id=?",
                [&grant.grant_id],
            )?;
        }
        tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'probe.started',?)",params![device,slot.grant.as_ref().map(|g|&g.grant_id),challenge.nonce])?;
        tx.commit()?;
        let observation =
            tokio::time::timeout(Duration::from_secs(5), probe.inspect(&challenge)).await;
        let observation = match observation {
            Ok(Ok(value)) => value,
            _ => return Err(BrokerError::EvidenceRequired),
        };
        if observation.device_id != device
            || observation.challenge_nonce != challenge.nonce
            || observation.prior_grant_id != slot.grant.as_ref().map(|g| g.grant_id.clone())
            || !valid_digest(&observation.evidence_digest)
            || !(observation.all_managed_tasks_joined
                && observation.all_managed_processes_exited
                && observation.device_probe_available
                && observation.device_healthy)
        {
            return Err(BrokerError::EvidenceRequired);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(grant) = &slot.grant {
            tx.execute(
                "UPDATE grants SET state='released' WHERE grant_id=?",
                [&grant.grant_id],
            )?;
        }
        let generation = slot
            .slot_generation
            .checked_add(1)
            .ok_or(BrokerError::RecoveryRequired)?;
        tx.execute("UPDATE slots SET state='idle',grant_id=NULL,generation=?,reason=NULL WHERE device_id=?",params![generation,device])?;
        tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'slot.reconciled',?)",params![device,slot.grant.as_ref().map(|g|&g.grant_id),json(&observation)?])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
