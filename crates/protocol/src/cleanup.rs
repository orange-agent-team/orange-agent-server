//! Wire evidence is a declaration by the authenticated CPU adapter, not a GPU
//! probe or permission to kill/recover a process by PID.
use crate::{execution::acknowledge_digest, grpc::worker, request_digest, valid_digest, valid_id};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupGrant {
    pub grant_id: String,
    pub device_id: String,
    pub broker_epoch: i64,
    pub slot_generation: i64,
    pub supervisor_instance: String,
    pub mode: String,
    pub profile_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuExit {
    pub instance_id: String,
    pub model_version: String,
    pub executable_sha256: String,
    pub model_sha256: String,
    pub phase: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub cpu_only: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuCleanupEvidence {
    pub schema_version: u32,
    pub grant: CleanupGrant,
    pub process: CpuExit,
    pub local_cleanup_digest: String,
}
impl CpuCleanupEvidence {
    pub fn valid(&self) -> bool {
        let g = &self.grant;
        let p = &self.process;
        self.schema_version == 1
            && g.device_id == "cpu"
            && g.mode == "remote"
            && g.broker_epoch > 0
            && g.slot_generation > 0
            && [
                &g.grant_id,
                &g.supervisor_instance,
                &g.profile_id,
                &p.instance_id,
                &p.model_version,
            ]
            .iter()
            .all(|s| valid_id(s))
            && p.phase == "stopped"
            && p.cpu_only
            && p.pid.is_none_or(|p| p > 0)
            && [
                &p.executable_sha256,
                &p.model_sha256,
                &self.local_cleanup_digest,
            ]
            .iter()
            .all(|s| valid_digest(s))
            && request_digest(
                "worker.cpu.cleanup.v1",
                &serde_json::json!({"grant":g,"process":p,"tasks_joined":true}),
            )
            .ok()
            .as_deref()
                == Some(self.local_cleanup_digest.as_str())
    }
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 16384 {
            return None;
        }
        let value =
            crate::canonical::parse_tool_arguments(std::str::from_utf8(bytes).ok()?).ok()?;
        let evidence: Self = serde_json::from_value(value).ok()?;
        evidence.valid().then_some(evidence)
    }
}
pub fn cleanup_digest(
    ack: &worker::Acknowledge,
    a: &worker::Assignment,
    e: &CpuCleanupEvidence,
    outcome: &str,
) -> Option<String> {
    if !e.valid()
        || !matches!(
            outcome,
            "delivered" | "delivery_unknown" | "failed" | "cancelled" | "authority_lost"
        )
        || ack.result != worker::AckResult::Accepted as i32
        || e.grant.grant_id != ack.broker_grant_id
        || e.grant.broker_epoch as u64 != ack.broker_epoch
        || e.grant.slot_generation as u64 != ack.slot_generation
        || e.grant.profile_id != a.execution_profile_id
        || e.process.model_version != a.model_version_id
        || e.process.model_sha256 != a.model_sha256
    {
        return None;
    }
    let digest = acknowledge_digest(ack, a)?;
    request_digest(
        "worker.cpu.cleanup.request.v1",
        &serde_json::json!({"accepted_digest":digest,"evidence":e,"outcome":outcome}),
    )
    .ok()
}
