//! Owned CPU execution of one authorized assignment. Model output stays an
//! unsubmitted candidate; no cloud success, settlement or GPU-cleanup claim.
use crate::{
    broker::*,
    control::{
        ControlConnection, ControlError, ExecutionPreparation, PendingRenewal, VerifiedRenewal,
    },
    execution_journal::{ExecutionJournal, ExecutionJournalError, ExecutionRecord, RejectionKind},
    node_journal::NodeJournal,
};
use orange_model_runtime::{
    ResponseCandidate,
    supervisor::{CpuProfile, ManagedCpuModel, ProcessRecord},
};
use orange_protocol::request_digest;
use serde::Serialize;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, thiserror::Error)]
pub enum CpuError {
    #[error("invalid approved CPU profile or assignment")]
    Invalid,
    #[error("CPU execution owner unavailable")]
    Unavailable,
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Journal(#[from] ExecutionJournalError),
}
pub struct CpuResources {
    pub connection: ControlConnection,
    pub node_journal: NodeJournal,
    pub journal: ExecutionJournal,
    pub broker: Broker,
    pub supervisor: Supervisor,
}
pub struct CpuExecutionSpec {
    pub command_id: String,
    pub profile_id: String,
    pub profile: CpuProfile,
    pub runtime_directory: PathBuf,
    pub capability_snapshot: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpuPhase {
    Preparing,
    Fetching,
    Starting,
    Generating,
    Stopping,
    Finished,
}
#[derive(Debug, Serialize)]
pub struct CpuRunReport {
    pub command_id: String,
    pub instance_id: String,
    pub outcome: String,
    pub response: Option<ResponseCandidate>,
    pub cleanup_confirmed: bool,
    pub authority_checks: u64,
    pub supervision_renewals: u64,
    pub cloud_renewals: u64,
    pub node_policy: Option<AppliedNodePolicy>,
    pub node_policy_error: Option<String>,
    pub cloud_completion: String,
    pub error_code: Option<String>,
}
pub struct CpuWorkerExit {
    pub report: CpuRunReport,
    pub resources: CpuResources,
}
pub struct CpuWorkerHandle {
    cancel: watch::Sender<bool>,
    progress: watch::Receiver<CpuPhase>,
    result: Option<oneshot::Receiver<Result<CpuWorkerExit, CpuError>>>,
}
impl Drop for CpuWorkerHandle {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}
impl CpuWorkerHandle {
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
    pub fn phase(&self) -> CpuPhase {
        self.progress.borrow().clone()
    }
    pub fn phases(&self) -> watch::Receiver<CpuPhase> {
        self.progress.clone()
    }
    pub async fn wait(&mut self) -> Result<CpuWorkerExit, CpuError> {
        let result = self.result.as_mut().ok_or(CpuError::Unavailable)?.await;
        self.result = None;
        result.map_err(|_| CpuError::Unavailable)?
    }
}
/// This digest binds the approved local executable and actual execution options,
/// not a caller-provided profile alias. Paths are installation-specific and are
/// verified by the existing supervisor against these hashes before spawning.
pub fn cpu_profile_digest(profile: &CpuProfile) -> Result<String, CpuError> {
    request_digest("orange-owned-cpu-v1",&serde_json::json!({"backend_sha256":profile.executable_sha256,"model_sha256":profile.model_sha256,"model_bytes":profile.model_bytes.to_string(),"model_version":profile.model_version,"context_tokens":profile.context_tokens,"startup_timeout_ms":profile.startup_timeout.as_millis().to_string(),"gpu_layers":0,"parallel":1,"threads":2})).map_err(|_|CpuError::Invalid)
}
pub fn spawn_cpu(resources: CpuResources, spec: CpuExecutionSpec) -> CpuWorkerHandle {
    let (cancel, receiver) = watch::channel(false);
    let (progress, phases) = watch::channel(CpuPhase::Preparing);
    let (send, result) = oneshot::channel();
    let stop = cancel.clone();
    // Dropping the public handle cancels, but never aborts the ownership task.
    // It retains Broker/journals until its model task and cleanup have settled.
    tokio::spawn(async move {
        let result = run(resources, spec, stop, receiver, progress).await;
        let _ = send.send(result);
    });
    CpuWorkerHandle {
        cancel,
        progress: phases,
        result: Some(result),
    }
}
struct EngineResult {
    response: Option<ResponseCandidate>,
    stopped: Option<ProcessRecord>,
    cancelled: bool,
    error: Option<&'static str>,
}
async fn engine(
    root: PathBuf,
    profile: CpuProfile,
    instance: String,
    request: orange_protocol::model::GenerateRequest,
    cancel: watch::Receiver<bool>,
    ready: mpsc::Sender<()>,
) -> EngineResult {
    let mut model =
        match ManagedCpuModel::launch_from_host(&root, profile, cancel.clone(), Some(instance))
            .await
        {
            Ok(model) => model,
            Err(_) => {
                return EngineResult {
                    response: None,
                    stopped: None,
                    cancelled: *cancel.borrow(),
                    error: Some("STARTUP_FAILED"),
                };
            }
        };
    let _ = ready.send(()).await;
    let (response, error) = match model.client() {
        Ok(client) => match client.generate(&request, cancel.clone()).await {
            Ok(response) => (Some(response), None),
            Err(orange_model_runtime::ModelError::Cancelled) => (None, Some("MODEL_CANCELLED")),
            Err(orange_model_runtime::ModelError::Limit) => (None, Some("MODEL_LIMIT")),
            Err(_) => (None, Some("MODEL_FAILED")),
        },
        Err(_) => (None, Some("MODEL_UNAVAILABLE")),
    };
    let stopped = match model.shutdown().await {
        Ok(()) => Some(model.record().clone()),
        Err(_) => None,
    };
    EngineResult {
        response,
        stopped,
        cancelled: *cancel.borrow(),
        error,
    }
}
async fn authority_cycle(
    connection: &mut ControlConnection,
    node: &mut NodeJournal,
    record: &ExecutionRecord,
    snapshot: &str,
    not_before: Instant,
    renewal: Option<PendingRenewal>,
) -> Result<(bool, Option<VerifiedRenewal>), ControlError> {
    tokio::time::sleep_until(tokio::time::Instant::from_std(not_before)).await;
    let intents = connection.heartbeat_node_control(snapshot, node).await?;
    let stop = !intents.is_empty()
        || connection.policy_changed_since(record.assignment.node_policy_generation);
    if !stop {
        connection.check_cpu_authority(record).await?;
    }
    let renewed = if !stop {
        match renewal {
            Some(pending) => Some(connection.renew_execution(pending).await?),
            None => None,
        }
    } else {
        None
    };
    Ok((stop, renewed))
}
async fn run(
    mut r: CpuResources,
    spec: CpuExecutionSpec,
    cancel: watch::Sender<bool>,
    mut user_cancel: watch::Receiver<bool>,
    progress: watch::Sender<CpuPhase>,
) -> Result<CpuWorkerExit, CpuError> {
    let record = r
        .journal
        .record(&spec.command_id)?
        .ok_or(CpuError::Invalid)?;
    let profile_digest = cpu_profile_digest(&spec.profile)?;
    if record.state != "received"
        || record.assignment.input_artifacts.len() != 1
        || spec.profile_id != record.assignment.execution_profile_id
        || spec.profile.model_version != record.assignment.model_version_id
        || spec.profile.model_sha256 != record.assignment.model_sha256
        || profile_digest != record.assignment.execution_profile_digest
    {
        return Err(CpuError::Invalid);
    }
    if *user_cancel.borrow() {
        return Err(CpuError::Unavailable);
    }
    let grant = match r.connection.prepare_execution(
        &mut r.journal,
        &mut r.broker,
        &r.supervisor,
        "cpu",
        &spec.command_id,
    )? {
        ExecutionPreparation::Reserved(grant) => grant,
        ExecutionPreparation::Rejected(decision) => {
            r.connection
                .send_rejection(&mut r.journal, &spec.command_id)
                .await?;
            let _ = progress.send(CpuPhase::Finished);
            return Ok(CpuWorkerExit {
                report: CpuRunReport {
                    command_id: spec.command_id,
                    instance_id: String::new(),
                    outcome: match decision.kind {
                        RejectionKind::Resource => "resource_rejected",
                        RejectionKind::Protocol => "profile_rejected",
                    }
                    .into(),
                    response: None,
                    cleanup_confirmed: false,
                    authority_checks: 0,
                    supervision_renewals: 0,
                    cloud_renewals: 0,
                    node_policy: None,
                    node_policy_error: None,
                    cloud_completion: "not_started".into(),
                    error_code: None,
                },
                resources: r,
            });
        }
    };
    let authorized = match r
        .connection
        .authorize_execution(
            &mut r.journal,
            &mut r.node_journal,
            &spec.capability_snapshot,
            &spec.command_id,
        )
        .await
    {
        Ok(value) => value,
        Err(error) => {
            r.broker.begin_drain(&r.supervisor, &grant)?;
            return Err(error.into());
        }
    };
    let started = authorized.confirm(&mut r.journal, &mut r.broker, &r.supervisor)?;
    let _ = progress.send(CpuPhase::Fetching);
    let request = match r
        .connection
        .fetch_model_input(&mut r.journal, &started, &spec.command_id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            r.broker.begin_drain(&r.supervisor, &grant)?;
            return Err(error.into());
        }
    };
    r.journal.bind_cpu_root(&spec.runtime_directory)?;
    let instance = uuid::Uuid::new_v4().to_string();
    r.journal.begin_cpu(
        r.connection.identity(),
        &spec.command_id,
        &instance,
        &record.assignment.request_projection_digest,
        &profile_digest,
    )?;
    let record = r
        .journal
        .record(&spec.command_id)?
        .ok_or(CpuError::Invalid)?;
    let _ = progress.send(CpuPhase::Starting);
    let (ready, mut readiness) = mpsc::channel(1);
    let mut task = tokio::spawn(engine(
        spec.runtime_directory.clone(),
        spec.profile,
        instance.clone(),
        request,
        user_cancel.clone(),
        ready,
    ));
    let mut local_tick = tokio::time::interval(Duration::from_secs(1));
    local_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut completed: Option<(EngineResult, Instant)> = None;
    let mut checks = 0;
    let mut cloud_renewals = 0;
    let mut next_renewal = Instant::now();
    let mut renewals = 0;
    let mut authority_lost = false;
    let mut requested_cancel = false;
    let mut next_check = Instant::now();
    let mut generating = false;
    let mut node_policy = None;
    let mut node_policy_error = None;
    'owner: loop {
        let cycle_started = next_check;
        let pending = if r.connection.supports_renewal() && Instant::now() >= next_renewal {
            match r
                .connection
                .prepare_renewal(&mut r.journal, &spec.command_id)
            {
                Ok(pending) => Some(pending),
                Err(_) => {
                    authority_lost = true;
                    break 'owner;
                }
            }
        } else {
            None
        };
        let network = {
            let cycle = authority_cycle(
                &mut r.connection,
                &mut r.node_journal,
                &record,
                &spec.capability_snapshot,
                next_check,
                pending,
            );
            tokio::pin!(cycle);
            loop {
                tokio::select! {
                    biased;
                    _=user_cancel.changed()=>{requested_cancel=true;let _=cancel.send(true);break 'owner;}
                    _=local_tick.tick()=>{
                        if *user_cancel.borrow(){requested_cancel=true;break 'owner;}
                        if r.broker.renew(&r.supervisor,&grant,None).is_err(){authority_lost=true;let _=cancel.send(true);break 'owner;}
                        renewals+=1;
                    }
                    value=readiness.recv(),if !generating=>{
                        if value.is_some(){if r.journal.cpu_phase(&spec.command_id,"generating").is_err(){authority_lost=true;let _=cancel.send(true);break 'owner;}let _=progress.send(CpuPhase::Generating);}generating=true;
                    }
                    result=&mut task,if completed.is_none()=>{
                        match result {Ok(result)=>completed=Some((result,Instant::now())),Err(_)=>{completed=Some((EngineResult{response:None,stopped:None,cancelled:true,error:Some("MODEL_TASK_FAILED")},Instant::now()));authority_lost=true;let _=cancel.send(true);break 'owner;}}
                    }
                    result=&mut cycle=>break result,
                }
            }
        };
        match network {
            Ok((false, renewed)) => {
                checks += 1;
                if let Some(renewed) = renewed {
                    if r.connection
                        .confirm_renewal(&mut r.journal, &mut r.broker, &r.supervisor, renewed)
                        .is_err()
                    {
                        authority_lost = true;
                        break;
                    }
                    cloud_renewals += 1;
                    next_renewal = Instant::now() + Duration::from_secs(10);
                }
            }
            Ok((true, _)) => {
                authority_lost = true;
                let _ = cancel.send(true);
                let _ = progress.send(CpuPhase::Stopping);
                // The model task is already cancelling. Quarantine/drain and
                // current policy authentication must never imply process exit.
                if r.broker.set_remote_allowed(false).is_err()
                    || r.broker.begin_drain(&r.supervisor, &grant).is_err()
                {
                    node_policy_error = Some("LOCAL_DRAIN_FAILED".into());
                } else {
                    match r
                        .connection
                        .authorize_latest_node_stop(&spec.capability_snapshot, &mut r.node_journal)
                        .await
                    {
                        Ok(authorization) => match authorization.apply(&mut r.broker) {
                            Ok(applied) => node_policy = Some(applied),
                            Err(_) => node_policy_error = Some("POLICY_APPLY_FAILED".into()),
                        },
                        Err(_) => node_policy_error = Some("POLICY_AUTHORITY_UNCONFIRMED".into()),
                    }
                }
                break;
            }
            Err(_) => {
                authority_lost = true;
                let _ = cancel.send(true);
                break;
            }
        }
        if let Some((_, finished)) = &completed {
            if cycle_started >= *finished {
                break;
            }
            next_check = Instant::now();
        } else {
            next_check = Instant::now() + Duration::from_secs(2);
        }
    }
    let _ = progress.send(CpuPhase::Stopping);
    let _ = cancel.send(true);
    let drained = r.broker.begin_drain(&r.supervisor, &grant).is_ok();
    let engine = if let Some((result, _)) = completed {
        result
    } else {
        task.await.unwrap_or(EngineResult {
            response: None,
            stopped: None,
            cancelled: true,
            error: Some("MODEL_TASK_FAILED"),
        })
    };
    let cleanup = engine
        .stopped
        .as_ref()
        .filter(|p| p.instance_id == instance && p.phase == "stopped" && p.cpu_only);
    let mut response = if !authority_lost && !engine.cancelled && !requested_cancel {
        engine.response
    } else {
        None
    };
    let mut outcome = if authority_lost {
        "authority_lost"
    } else if engine.cancelled || requested_cancel {
        "cancelled"
    } else if response.is_some() {
        "computed_unsubmitted"
    } else {
        "failed"
    };
    let mut cleanup_digest = None;
    if let Some(record) = cleanup.filter(|_| drained) {
        let proof = CpuCleanupProof {
            grant: grant.clone(),
            record: record.clone(),
            root: spec.runtime_directory,
        };
        if r.broker
            .release(&r.supervisor, &grant, &proof)
            .await
            .is_ok()
        {
            cleanup_digest = Some(proof.digest()?);
        }
    }
    if cleanup_digest.is_none() {
        outcome = "cleanup_unknown";
        response = None;
    }
    let response_digest = response
        .as_ref()
        .map(|v| request_digest("worker.cpu.candidate.v1", v).map_err(|_| CpuError::Invalid))
        .transpose()?;
    let error_code = if outcome == "cleanup_unknown" {
        Some("CLEANUP_UNCONFIRMED")
    } else if authority_lost {
        Some("AUTHORITY_LOST")
    } else if requested_cancel {
        Some("USER_CANCELLED")
    } else {
        engine.error
    };
    r.journal.finish_cpu(
        &spec.command_id,
        outcome,
        response_digest.as_deref(),
        cleanup_digest.as_deref(),
        error_code,
    )?;
    let _ = progress.send(CpuPhase::Finished);
    Ok(CpuWorkerExit {
        report: CpuRunReport {
            command_id: spec.command_id,
            instance_id: instance,
            outcome: outcome.into(),
            response,
            cleanup_confirmed: cleanup_digest.is_some(),
            authority_checks: checks,
            supervision_renewals: renewals,
            cloud_renewals,
            node_policy,
            node_policy_error,
            cloud_completion: "pending_output_commit".into(),
            error_code: error_code.map(str::to_owned),
        },
        resources: r,
    })
}
struct CpuCleanupProof {
    grant: Grant,
    record: ProcessRecord,
    root: PathBuf,
}
impl CpuCleanupProof {
    fn digest(&self) -> Result<String, CpuError> {
        request_digest(
            "worker.cpu.cleanup.v1",
            &serde_json::json!({"grant":self.grant,"process":self.record,"tasks_joined":true}),
        )
        .map_err(|_| CpuError::Invalid)
    }
}
impl ResourceProbe for CpuCleanupProof {
    fn inspect<'a>(&'a self, challenge: &'a ProbeChallenge) -> ProbeFuture<'a> {
        Box::pin(async move {
            if challenge.prior_grant.as_ref() != Some(&self.grant)
                || challenge.device_id != "cpu"
                || challenge.broker_epoch != self.grant.broker_epoch
                || challenge.slot_generation != self.grant.slot_generation
                || self.record.phase != "stopped"
                || !self.record.cpu_only
            {
                return Err(BrokerError::EvidenceRequired);
            }
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(self.root.join("engine.lease"))?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
                return Err(BrokerError::EvidenceRequired);
            }
            file.try_lock().map_err(|_| BrokerError::EvidenceRequired)?;
            Ok(ProbeObservation {
                device_id: challenge.device_id.clone(),
                challenge_nonce: challenge.nonce.clone(),
                prior_grant_id: Some(self.grant.grant_id.clone()),
                all_managed_tasks_joined: true,
                all_managed_processes_exited: true,
                device_probe_available: true,
                device_healthy: true,
                evidence_digest: self.digest().map_err(|_| BrokerError::EvidenceRequired)?,
            })
        })
    }
}
