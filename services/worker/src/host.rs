//! Development-only persistent Unix CPU owner. No enrollment, output commit,
//! automatic orphan recovery, arbitrary peer authentication or GPU certificate.
mod config;
mod transcription_profile;
pub use transcription_profile::probe as probe_transcription;
#[cfg(test)]
mod tests;
use crate::{
    broker::*,
    control::{ControlConfig, ControlConnection, ControlError},
    cpu::{
        CpuExecutionSpec, CpuResources, TranscriptionExecutionSpec, cpu_profile_digest, spawn_cpu,
        spawn_transcription,
    },
    execution_journal::{ExecutionJournal, ExecutionJournalError},
    node_journal::{JournalError, NodeJournal},
};
pub use config::WorkerConfig;
use std::{
    fs::File,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::Duration,
};
use tokio::sync::{mpsc, watch};
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("invalid development Worker configuration or missing explicit contribution consent")]
    Configuration,
    #[error("private current-user state or owned CPU slot unavailable")]
    PrivateState,
    #[error("Worker control unavailable; persisted uncertainty requires review")]
    Control(#[from] ControlError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Journal(#[from] ExecutionJournalError),
    #[error(transparent)]
    Node(#[from] JournalError),
    #[error(transparent)]
    Cpu(#[from] crate::cpu::CpuError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
/// Content-free operator event. Candidate text, prompts and TLS secrets are not
/// written to stdout. A computed result is still not delivered or settled.
#[derive(serde::Serialize)]
pub struct HostEvent {
    pub protocol: u32,
    pub state: &'static str,
    pub command_id: Option<String>,
    pub outcome: Option<String>,
    pub cleanup_confirmed: bool,
    pub policy_applied: bool,
}
fn event(state: &'static str) -> HostEvent {
    HostEvent {
        protocol: 1,
        state,
        command_id: None,
        outcome: None,
        cleanup_confirmed: false,
        policy_applied: false,
    }
}
fn emit(sender: &mpsc::Sender<HostEvent>, value: HostEvent) {
    // Telemetry backpressure must not freeze supervision or graceful shutdown.
    // Durable execution/Broker journals, not this best-effort stream, are truth.
    let _ = sender.try_send(value);
}
async fn flush_cleanups(
    resources: &mut CpuResources,
    config: &WorkerConfig,
    capabilities: &str,
    events: &mpsc::Sender<HostEvent>,
) -> Result<(), HostError> {
    if !config.confirm_cleanup {
        return Ok(());
    }
    for command in resources.journal.pending_cleanups()? {
        if !resources.connection.is_current_locally() {
            let fresh = if config
                .transcription_observation
                .as_ref()
                .is_some_and(|o| o.allow_remote_execution)
            {
                ControlConnection::connect_managed_transcription(
                    config.control(capabilities)?,
                    &mut resources.node_journal,
                    &mut resources.journal,
                )
                .await?
            } else {
                ControlConnection::connect_managed_cpu(
                    config.control(capabilities)?,
                    &mut resources.node_journal,
                    &mut resources.journal,
                    config.commit_outputs,
                )
                .await?
            };
            std::mem::replace(&mut resources.connection, fresh).close();
        }
        resources
            .connection
            .confirm_cpu_cleanup(&mut resources.journal, &command)
            .await?;
        emit(
            events,
            HostEvent {
                command_id: Some(command),
                cleanup_confirmed: true,
                ..event("cloud_cleanup_confirmed")
            },
        );
    }
    Ok(())
}
struct EmptyCpuAdmission {
    root: PathBuf,
}
impl ResourceProbe for EmptyCpuAdmission {
    fn inspect<'a>(&'a self, c: &'a ProbeChallenge) -> ProbeFuture<'a> {
        Box::pin(async move {
            if c.device_id != "cpu" || c.prior_grant.is_some() {
                return Err(BrokerError::RecoveryRequired);
            }
            config::private_directory(&self.root).map_err(|_| BrokerError::StoragePolicy)?;
            let record = self.root.join("process.json");
            match std::fs::symlink_metadata(&record) {
                Ok(_) => {
                    let bytes = config::private_file(&record, 16 * 1024)
                        .map_err(|_| BrokerError::EvidenceRequired)?;
                    let record: orange_model_runtime::supervisor::ProcessRecord =
                        serde_json::from_slice(&bytes)
                            .map_err(|_| BrokerError::EvidenceRequired)?;
                    if record.phase != "stopped" || !record.cpu_only {
                        return Err(BrokerError::RecoveryRequired);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let mut slots = Vec::new();
            let mut identities = Vec::new();
            for name in ["engine.lease", "transcription.lock"] {
                let lease = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(self.root.join(name))?;
                let m = lease.metadata()?;
                if !m.is_file()
                    || m.uid() != config::uid()
                    || m.nlink() != 1
                    || m.mode() & 0o077 != 0
                {
                    return Err(BrokerError::StoragePolicy);
                }
                lease.try_lock().map_err(|_| BrokerError::Occupied)?;
                identities.push((name, m.dev().to_string(), m.ino().to_string()));
                slots.push(lease);
            }
            Ok(ProbeObservation {
                device_id: c.device_id.clone(),
                challenge_nonce: c.nonce.clone(),
                prior_grant_id: None,
                all_managed_tasks_joined: true,
                all_managed_processes_exited: true,
                device_probe_available: true,
                device_healthy: true,
                evidence_digest: orange_protocol::request_digest(
                    "worker.empty-owned-cpu.v1",
                    &serde_json::json!({"root":self.root,"slots":identities,"challenge":c.nonce}),
                )
                .map_err(|_| BrokerError::Invalid)?,
            })
        })
    }
}
/// Run until an explicit shutdown, a fail-closed control error or unreconciled
/// execution. The caller must await this owner; never abort it to stop a model.
pub async fn run(
    config: WorkerConfig,
    mut stop: watch::Receiver<bool>,
    events: mpsc::Sender<HostEvent>,
) -> Result<(), HostError> {
    config.validate()?;
    let root = config::private_directory(&config.state_directory)?;
    let model_root = root.join("model");
    if !model_root.try_exists()? {
        std::fs::DirBuilder::new().mode(0o700).create(&model_root)?;
    }
    config::private_directory(&model_root)?;
    let profile_digest = cpu_profile_digest(&config.profile.model())?;
    let capabilities=orange_protocol::request_digest("worker.cpu.capabilities.v1",&serde_json::json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"profile":profile_digest,"cpu_slots":1,"gpu_slots":0})).map_err(|_|HostError::Configuration)?;
    // Same-process adapter: owner comes from the OS, fresh process nonce from
    // the system RNG. No caller-supplied owner or PID-only recovery identity.
    let owner = format!("unix-uid-{}", config::uid());
    let instance = uuid::Uuid::new_v4().to_string();
    let process = format!(
        "self-{}-{}-{}",
        config::uid(),
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let peer = VerifiedPeer::from_verified_os_identity(
        owner.clone(),
        process,
        instance,
        uuid::Uuid::new_v4().simple().to_string(),
    )?;
    let mut journal =
        ExecutionJournal::open(&root.join("execution.sqlite"), config.node_id.clone())?;
    journal.bind_cpu_root(&model_root)?;
    let mut node = NodeJournal::open(&root.join("node.sqlite"), config.node_id.clone())?;
    let mut approved_profiles = vec![ApprovedProfile {
        id: config.profile_id.clone(),
        model_sha256: config.profile.model_sha256.clone(),
        execution_digest: profile_digest,
    }];
    let observation = if let Some(settings) = &config.transcription_observation {
        let slot = settings
            .allow_remote_execution
            .then_some(model_root.as_path());
        let result =
            transcription_profile::prepare(&settings.private_probe_file, slot, stop.clone()).await;
        if *stop.borrow() {
            return Ok(());
        }
        match result {
            Ok((client, fact)) => {
                if settings.allow_remote_execution {
                    approved_profiles.push(ApprovedProfile {
                        id: settings.execution_profile_id.clone(),
                        model_sha256: fact.profile.model_sha256.clone(),
                        execution_digest: fact.execution_profile_digest.clone(),
                    });
                }
                Some((
                    settings,
                    std::sync::Arc::new(client),
                    std::sync::Arc::new(fact),
                ))
            }
            Err(_) if settings.allow_remote_execution => return Err(HostError::Configuration),
            Err(_) => {
                emit(&events, event("transcription_probe_failed"));
                None
            }
        }
    } else {
        None
    };
    let mut broker = Broker::open(
        &root.join("broker.sqlite"),
        BrokerConfig {
            owner_identity: owner,
            devices: vec!["cpu".into()],
            approved_profiles,
        },
    )?;
    broker.bind_enrolled_node(&config.node_id)?;
    broker
        .reconcile(
            "cpu",
            &EmptyCpuAdmission {
                root: model_root.clone(),
            },
        )
        .await?;
    if !broker.node_policy()?.platform_blocked {
        broker.set_remote_allowed(config.allow_remote_contribution)?;
    }
    let supervisor = broker.register_supervisor(peer)?;
    if *stop.borrow() {
        return Ok(());
    }
    let connection = if observation
        .as_ref()
        .is_some_and(|(settings, _, _)| settings.allow_remote_execution)
    {
        ControlConnection::connect_managed_transcription(
            config.control(&capabilities)?,
            &mut node,
            &mut journal,
        )
        .await?
    } else if config.confirm_cleanup {
        ControlConnection::connect_managed_cpu(
            config.control(&capabilities)?,
            &mut node,
            &mut journal,
            config.commit_outputs,
        )
        .await?
    } else if config.commit_outputs {
        ControlConnection::connect_output_execution(
            config.control(&capabilities)?,
            &mut node,
            &mut journal,
        )
        .await?
    } else {
        ControlConnection::connect_renewable_execution(
            config.control(&capabilities)?,
            &mut node,
            &mut journal,
        )
        .await?
    };
    let mut resources = CpuResources {
        connection,
        node_journal: node,
        journal,
        broker,
        supervisor,
    };
    for command in resources.journal.unresolved_outputs()? {
        match resources
            .connection
            .query_output(&mut resources.journal, &command)
            .await
        {
            Ok(_) => emit(
                &events,
                HostEvent {
                    command_id: Some(command),
                    outcome: Some("response_committed".into()),
                    ..event("response_commit_recovered")
                },
            ),
            Err(error) => {
                if !config.confirm_cleanup
                    || !resources.journal.pending_cleanups()?.contains(&command)
                {
                    return Err(error.into());
                }
                emit(
                    &events,
                    HostEvent {
                        command_id: Some(command),
                        ..event("output_unconfirmed")
                    },
                );
            }
        }
    }
    flush_cleanups(&mut resources, &config, &capabilities, &events).await?;
    emit(&events, event("ready"));
    // Preserve an unsubmitted candidate only in this owner's memory. Until the
    // output protocol is implemented it cannot start another model or report
    // cloud success. A shutdown explicitly discards this uncommitted value.
    let mut pending_output = None;
    let mut observed_session = None;
    loop {
        if *stop.borrow() {
            break;
        }
        if let Some((settings, _, fact)) = &observation
            && observed_session.as_deref()
                != Some(resources.connection.identity().session_id.as_str())
        {
            use crate::control::ObservationFailure;
            let result = resources
                .connection
                .report_transcription_profile(&settings.execution_profile_id, fact)
                .await;
            observed_session = Some(resources.connection.identity().session_id.clone());
            match result {
                Ok(_) => emit(&events, event("transcription_profile_observed")),
                Err(ObservationFailure::NotApproved | ObservationFailure::Unsupported) => {
                    emit(&events, event("transcription_profile_not_admitted"))
                }
                Err(ObservationFailure::Control(e)) => return Err(e.into()),
            }
        }
        let poll = tokio::select! {
            _=stop.changed()=>break,
            poll=resources.connection.poll_execution("cpu-host",&mut resources.node_journal,&mut resources.journal)=>poll?,
        };
        let local_generation = resources
            .broker
            .node_policy()?
            .policy_generation
            .parse::<u64>()
            .map_err(|_| HostError::PrivateState)?;
        if !poll.node_intents.is_empty()
            || resources.connection.policy_changed_since(local_generation)
        {
            resources.broker.set_remote_allowed(false)?;
            resources
                .connection
                .authorize_latest_node_stop("cpu-host", &mut resources.node_journal)
                .await?
                .apply(&mut resources.broker)?;
            emit(
                &events,
                HostEvent {
                    policy_applied: true,
                    ..event("policy_applied")
                },
            );
        }
        if let Some(record) = poll.assignment {
            if pending_output.is_some() {
                return Err(HostError::Configuration);
            }
            let mut execution = if record.assignment.transcription_input.is_some() {
                let (settings, client, fact) = observation
                    .as_ref()
                    .filter(|(s, _, _)| s.allow_remote_execution)
                    .ok_or(HostError::Configuration)?;
                spawn_transcription(
                    resources,
                    TranscriptionExecutionSpec {
                        command_id: record.command_id,
                        profile_id: settings.execution_profile_id.clone(),
                        client: client.clone(),
                        qualification: fact.clone(),
                        runtime_directory: model_root.clone(),
                        capability_snapshot: "cpu-host".into(),
                    },
                )
            } else {
                let spec = CpuExecutionSpec {
                    command_id: record.command_id,
                    profile_id: config.profile_id.clone(),
                    profile: config.profile.model(),
                    runtime_directory: model_root.clone(),
                    capability_snapshot: "cpu-host".into(),
                };
                spawn_cpu(resources, spec)
            };
            let exit = tokio::select! {
                _=stop.changed()=>{execution.cancel();execution.wait().await?},
                result=execution.wait()=>result?,
            };
            emit(
                &events,
                HostEvent {
                    protocol: 1,
                    state: "execution_stopped",
                    command_id: Some(exit.report.command_id.clone()),
                    outcome: Some(exit.report.outcome.clone()),
                    cleanup_confirmed: exit.report.cleanup_confirmed,
                    policy_applied: exit.report.node_policy.is_some(),
                },
            );
            pending_output = exit.report.response;
            resources = exit.resources;
            if !exit.report.cleanup_confirmed {
                return Err(HostError::PrivateState);
            }
            if let Some(transcript) = exit.report.transcript {
                let result = resources
                    .connection
                    .commit_transcription_output(
                        &mut resources.journal,
                        &exit.report.command_id,
                        &transcript,
                    )
                    .await;
                emit(
                    &events,
                    HostEvent {
                        command_id: Some(exit.report.command_id.clone()),
                        cleanup_confirmed: true,
                        ..event(if result.is_ok() {
                            "response_committed"
                        } else {
                            "output_unconfirmed"
                        })
                    },
                );
            }
            if pending_output.is_some() {
                if config.commit_outputs {
                    let response = pending_output.as_ref().ok_or(HostError::Configuration)?;
                    let result = resources
                        .connection
                        .commit_output(&mut resources.journal, &exit.report.command_id, response)
                        .await;
                    pending_output = None;
                    match result {
                        Ok(_) => emit(
                            &events,
                            HostEvent {
                                command_id: Some(exit.report.command_id.clone()),
                                outcome: Some("response_committed".into()),
                                cleanup_confirmed: true,
                                ..event("response_committed")
                            },
                        ),
                        Err(error) => {
                            if !config.confirm_cleanup {
                                return Err(error.into());
                            }
                            emit(
                                &events,
                                HostEvent {
                                    command_id: Some(exit.report.command_id.clone()),
                                    ..event("output_unconfirmed")
                                },
                            );
                        }
                    }
                } else {
                    emit(&events, event("pending_output_commit"));
                }
            }
        }
        if pending_output.is_none() {
            flush_cleanups(&mut resources, &config, &capabilities, &events).await?;
        }
        tokio::select! {_=stop.changed()=>break,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
    }
    if pending_output.take().is_some() {
        emit(&events, event("unsubmitted_output_discarded"));
    }
    flush_cleanups(&mut resources, &config, &capabilities, &events).await?;
    resources.connection.close();
    emit(&events, event("stopped"));
    Ok(())
}
