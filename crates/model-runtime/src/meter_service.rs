//! Server-owned counting authority. No caller URL/native usage can mint facts.
use crate::{GenerateRequest, ResponseCandidate, metering};
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
#[derive(Debug, thiserror::Error)]
pub enum MeterError {
    #[error("meter profile/request invalid")]
    Invalid,
    #[error("trusted meter busy")]
    Busy,
    #[error("trusted meter unavailable or not safely stopped")]
    Unavailable,
    #[error("canonical output limit exceeded")]
    Limit,
}
#[derive(Clone, Debug, Serialize)]
pub struct MeterDescriptor {
    pub profile_digest: String,
    pub execution_digest: String,
    pub model_version: String,
    pub model_sha256: String,
    pub executable_sha256: String,
    pub context_tokens: u32,
    pub input_rule: &'static str,
    pub output_rule: &'static str,
    pub tool_schema_rule: &'static str,
}
/// Serialize for durable evidence, but never Deserialize or construct externally.
#[derive(Clone, Debug, Serialize)]
pub struct MeasuredInput {
    profile_digest: String,
    request_digest: String,
    input_units: u64,
}
impl MeasuredInput {
    pub fn profile_digest(&self) -> &str {
        &self.profile_digest
    }
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }
    pub fn units(&self) -> u64 {
        self.input_units
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct MeasuredOutput {
    input: MeasuredInput,
    response_digest: String,
    projection_digest: String,
    output_units: u64,
    has_tools: bool,
}
impl MeasuredOutput {
    pub fn input(&self) -> &MeasuredInput {
        &self.input
    }
    pub fn response_digest(&self) -> &str {
        &self.response_digest
    }
    pub fn projection_digest(&self) -> &str {
        &self.projection_digest
    }
    pub fn units(&self) -> u64 {
        self.output_units
    }
    pub fn has_tools(&self) -> bool {
        self.has_tools
    }
}
enum Command {
    Input(
        GenerateRequest,
        oneshot::Sender<Result<MeasuredInput, MeterError>>,
        tokio::time::Instant,
    ),
    Output(
        GenerateRequest,
        ResponseCandidate,
        u64,
        oneshot::Sender<Result<MeasuredOutput, MeterError>>,
        tokio::time::Instant,
    ),
}
struct Inner {
    descriptor: MeterDescriptor,
    sender: mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
    ended: Mutex<Option<oneshot::Receiver<Result<(), MeterError>>>>,
    stopped: AtomicBool,
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
#[derive(Clone)]
pub struct TrustedMeter {
    inner: Arc<Inner>,
}
impl TrustedMeter {
    #[cfg(unix)]
    pub async fn launch(
        root: &std::path::Path,
        profile: crate::supervisor::CpuProfile,
    ) -> Result<Self, MeterError> {
        let execution_digest = profile
            .execution_digest()
            .map_err(|_| MeterError::Invalid)?;
        let input_rule = "llama-chat-template-input-v1";
        let profile_digest=orange_protocol::request_digest("orange.trusted-meter.v1",&serde_json::json!({"execution_digest":execution_digest,"input_rule":input_rule,"output_rule":metering::PROFILE,"tool_schema_rule":crate::tool_schema::RULE})).map_err(|_|MeterError::Invalid)?;
        let descriptor = MeterDescriptor {
            profile_digest,
            execution_digest,
            model_version: profile.model_version.clone(),
            model_sha256: profile.model_sha256.clone(),
            executable_sha256: profile.executable_sha256.clone(),
            context_tokens: profile.context_tokens,
            input_rule,
            output_rule: metering::PROFILE,
            tool_schema_rule: crate::tool_schema::RULE,
        };
        let (stop, mut stopping) = watch::channel(false);
        let mut model = crate::supervisor::ManagedCpuModel::launch_from_host(
            root,
            profile,
            stopping.clone(),
            Some(uuid::Uuid::new_v4().to_string()),
        )
        .await
        .map_err(|_| MeterError::Unavailable)?;
        let (sender, mut commands) = mpsc::channel(8);
        let (finished, ended) = oneshot::channel();
        let identity = descriptor.clone();
        tokio::spawn(async move {
            let mut health = tokio::time::interval(Duration::from_secs(1));
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let command = tokio::select! {biased;_=stopping.changed()=>break,_=health.tick()=>{if !matches!(model.poll_status(),Ok(record) if record.phase=="ready"){break;}continue;},command=commands.recv()=>match command {Some(command)=>command,None=>break}};
                let ok = match command {
                    Command::Input(request, send, deadline) => {
                        if send.is_closed() {
                            continue;
                        }
                        if deadline <= tokio::time::Instant::now() {
                            let _ = send.send(Err(MeterError::Busy));
                            continue;
                        }
                        let result = tokio::select! {biased;_=stopping.changed()=>Err(MeterError::Unavailable),result=tokio::time::timeout_at(deadline,async {measure_input(model.client().map_err(|_|MeterError::Unavailable)?,&identity,&request).await})=>result.unwrap_or(Err(MeterError::Unavailable))};
                        let ok = !matches!(result, Err(MeterError::Unavailable));
                        let _ = send.send(result);
                        ok
                    }
                    Command::Output(request, candidate, limit, send, deadline) => {
                        if send.is_closed() {
                            continue;
                        }
                        if deadline <= tokio::time::Instant::now() {
                            let _ = send.send(Err(MeterError::Busy));
                            continue;
                        }
                        let result = tokio::select! {biased;_=stopping.changed()=>Err(MeterError::Unavailable),result=tokio::time::timeout_at(deadline,async {measure_output(model.client().map_err(|_|MeterError::Unavailable)?,&identity,&request,&candidate,limit).await})=>result.unwrap_or(Err(MeterError::Unavailable))};
                        let ok = !matches!(result, Err(MeterError::Unavailable));
                        let _ = send.send(result);
                        ok
                    }
                };
                if !ok {
                    break;
                }
            }
            commands.close();
            let result = model.shutdown().await.map_err(|_| MeterError::Unavailable);
            let _ = finished.send(result);
        });
        Ok(Self {
            inner: Arc::new(Inner {
                descriptor,
                sender,
                stop,
                ended: Mutex::new(Some(ended)),
                stopped: AtomicBool::new(false),
            }),
        })
    }
    pub fn descriptor(&self) -> &MeterDescriptor {
        &self.inner.descriptor
    }
    pub fn available(&self) -> bool {
        !*self.inner.stop.borrow() && !self.inner.sender.is_closed()
    }
    pub async fn input(&self, request: GenerateRequest) -> Result<MeasuredInput, MeterError> {
        request.validate().map_err(|_| MeterError::Invalid)?;
        if *self.inner.stop.borrow() {
            return Err(MeterError::Unavailable);
        }
        let (send, result) = oneshot::channel();
        self.inner
            .sender
            .try_send(Command::Input(
                request,
                send,
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .map_err(|_| MeterError::Busy)?;
        result.await.map_err(|_| MeterError::Unavailable)?
    }
    pub async fn output(
        &self,
        request: GenerateRequest,
        candidate: ResponseCandidate,
        limit: u64,
    ) -> Result<MeasuredOutput, MeterError> {
        request.validate().map_err(|_| MeterError::Invalid)?;
        metering::project_candidate(&candidate).map_err(|_| MeterError::Invalid)?;
        if limit == 0 || limit > 4096 {
            return Err(MeterError::Invalid);
        }
        if *self.inner.stop.borrow() {
            return Err(MeterError::Unavailable);
        }
        let (send, result) = oneshot::channel();
        self.inner
            .sender
            .try_send(Command::Output(
                request,
                candidate,
                limit,
                send,
                tokio::time::Instant::now() + Duration::from_secs(10),
            ))
            .map_err(|_| MeterError::Busy)?;
        result.await.map_err(|_| MeterError::Unavailable)?
    }
    pub async fn shutdown(&self) -> Result<(), MeterError> {
        let _ = self.inner.stop.send(true);
        let mut ended = self.inner.ended.lock().await;
        if let Some(receiver) = ended.as_mut() {
            let result = receiver.await.unwrap_or(Err(MeterError::Unavailable));
            *ended = None;
            if result.is_ok() {
                self.inner.stopped.store(true, Ordering::Release);
            }
            result
        } else if self.inner.stopped.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(MeterError::Unavailable)
        }
    }
}
#[cfg(unix)]
async fn measure_input(
    client: &crate::llama::LlamaClient,
    d: &MeterDescriptor,
    r: &GenerateRequest,
) -> Result<MeasuredInput, MeterError> {
    crate::tool_schema::validate_request(r).map_err(|_| MeterError::Invalid)?;
    if r.model_version != d.model_version {
        return Err(MeterError::Invalid);
    }
    let input_units = client.input_tokens(r).await.map_err(|e| match e {
        crate::ModelError::Limit => MeterError::Limit,
        _ => MeterError::Unavailable,
    })?;
    Ok(MeasuredInput {
        profile_digest: d.profile_digest.clone(),
        request_digest: orange_protocol::request_digest("model-request-v1", r)
            .map_err(|_| MeterError::Invalid)?,
        input_units,
    })
}
#[cfg(unix)]
async fn measure_output(
    client: &crate::llama::LlamaClient,
    d: &MeterDescriptor,
    r: &GenerateRequest,
    c: &ResponseCandidate,
    limit: u64,
) -> Result<MeasuredOutput, MeterError> {
    crate::tool_schema::validate_response(r, c).map_err(|_| MeterError::Invalid)?;
    let projection = metering::project_candidate(c).map_err(|_| MeterError::Invalid)?;
    let input = measure_input(client, d, r).await?;
    let mut output_units = 0u64;
    for part in projection.parts() {
        output_units = output_units
            .checked_add(
                client
                    .tokenize(&part.text)
                    .await
                    .map_err(|_| MeterError::Unavailable)?
                    .len() as u64,
            )
            .ok_or(MeterError::Limit)?;
        if output_units > limit {
            return Err(MeterError::Limit);
        }
    }
    Ok(MeasuredOutput {
        input,
        response_digest: orange_protocol::request_digest("worker.cpu.candidate.v1", c)
            .map_err(|_| MeterError::Invalid)?,
        projection_digest: projection.digest().into(),
        output_units,
        has_tools: projection.has_tools(),
    })
}
