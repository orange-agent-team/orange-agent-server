//! Public Worker diagnostics. A matching self-report still requires an independent target qualification.
use crate::grpc::worker::IsolationObservation;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROFILE: &str = "wsl2-reviewed-text-v1";
pub const POLICY: &str = "2fa2e7a0b4597af9a30dfedbfec8c1a3500857f30d74d888aec4841d94d34fdb";
pub const PROBE: &str = "2d4afe4294922d1c2b4abb16ca4ab1ecf6cf2c038000d86ce93eca6c00f90544";
pub const MAX_BYTES: usize = 4096;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Presence {
    Present,
    Absent,
    PermissionDenied,
    Unobserved,
    CharacterDevice,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Facts {
    protocol: u32,
    status: String,
    uid_gid_match: bool,
    no_new_privileges: bool,
    seccomp_filter: bool,
    capabilities_empty: bool,
    root_readonly: bool,
    private_tmp: bool,
    private_control: bool,
    unix_round_trip: bool,
    unexpected_mounts: u32,
    device_leaf_mounts: u32,
    nonloopback_interfaces: u32,
    inet_socket: String,
    windows_volume: Presence,
    container_socket: Presence,
    containerd_socket: Presence,
    interop: Presence,
    gpu_bridge: Presence,
    process_policy_match: bool,
    readonly_fixture: String,
    descriptor_test: String,
    runtime_verified: bool,
    supply_authorized: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unavailable {
    protocol: u32,
    status: String,
    runtime_verified: bool,
    supply_authorized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    UnknownRelease,
}

pub struct Checked {
    pub generation: i64,
    pub data: Value,
    pub digest: String,
    pub disposition: &'static str,
}

fn classify(value: &Value) -> Result<&'static str, Error> {
    if value.get("status").and_then(Value::as_str) == Some("observation_unavailable") {
        let f: Unavailable = serde_json::from_value(value.clone()).map_err(|_| Error::Invalid)?;
        return if f.protocol == 1
            && f.status == "observation_unavailable"
            && !f.runtime_verified
            && !f.supply_authorized
        {
            Ok("incomplete")
        } else {
            Err(Error::Invalid)
        };
    }
    let f: Facts = serde_json::from_value(value.clone()).map_err(|_| Error::Invalid)?;
    if f.protocol != 1
        || f.runtime_verified
        || f.supply_authorized
        || f.device_leaf_mounts > 7
        || !matches!(
            f.inet_socket.as_str(),
            "creation_allowed" | "permission_denied" | "unobserved"
        )
        || !matches!(
            f.readonly_fixture.as_str(),
            "readonly_filesystem"
                | "permission_denied"
                | "write_open_allowed"
                | "fixture_absent"
                | "unexpected_fixture"
                | "unobserved"
                | "not_requested"
                | "unconfirmed_process_boundary"
        )
        || !matches!(
            f.descriptor_test.as_str(),
            "process_limit_released"
                | "process_descriptor_limit"
                | "limit_not_observed"
                | "unconfirmed_failure"
                | "unexpected_fixture"
                | "unobserved"
                | "not_requested"
                | "unconfirmed_process_boundary"
        )
    {
        return Err(Error::Invalid);
    }
    let process = f.uid_gid_match
        && f.no_new_privileges
        && f.seccomp_filter
        && f.capabilities_empty
        && f.root_readonly
        && f.private_tmp
        && f.private_control
        && f.unix_round_trip
        && f.unexpected_mounts == 0
        && f.nonloopback_interfaces == 0
        && f.inet_socket == "permission_denied"
        && [
            f.windows_volume,
            f.container_socket,
            f.containerd_socket,
            f.interop,
        ]
        .iter()
        .all(|v| matches!(v, Presence::Absent | Presence::PermissionDenied));
    let passive = f.readonly_fixture == "not_requested" && f.descriptor_test == "not_requested";
    let skipped = f.readonly_fixture == "unconfirmed_process_boundary"
        && f.descriptor_test == "unconfirmed_process_boundary";
    let bounded = f.readonly_fixture == "readonly_filesystem"
        && f.descriptor_test == "process_limit_released";
    if process != f.process_policy_match
        || (!process && !(skipped || passive))
        || (process && skipped)
        || (f.readonly_fixture == "not_requested") != (f.descriptor_test == "not_requested")
        || (f.readonly_fixture == "unconfirmed_process_boundary")
            != (f.descriptor_test == "unconfirmed_process_boundary")
    {
        return Err(Error::Invalid);
    }
    let matched = process && f.gpu_bridge == Presence::CharacterDevice && (bounded || passive);
    if f.status
        != if matched {
            "observations_match"
        } else {
            "observations_incomplete"
        }
    {
        return Err(Error::Invalid);
    }
    Ok(if matched && bounded {
        "independent_review_required"
    } else {
        "incomplete"
    })
}

pub fn check(request: &IsolationObservation) -> Result<Checked, Error> {
    let generation = request
        .node_policy_generation
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(Error::Invalid)?;
    if request.report_version != 1
        || !crate::valid_id(&request.session_id)
        || !crate::valid_id(&request.request_id)
        || request.reported_capture_unix <= 0
        || request.connection_epoch == 0
        || request.connection_epoch > i64::MAX as u64
        || request.probe_json.len() > MAX_BYTES
    {
        return Err(Error::Invalid);
    }
    if request.profile_id != PROFILE
        || request.oci_policy_sha256 != POLICY
        || request.linux_probe_sha256 != PROBE
    {
        return Err(Error::UnknownRelease);
    }
    let data = crate::canonical::parse_tool_arguments(
        std::str::from_utf8(&request.probe_json).map_err(|_| Error::Invalid)?,
    )
    .map_err(|_| Error::Invalid)?;
    let disposition = classify(&data)?;
    let digest = crate::request_digest(
        "worker.isolation-observation.v1",
        &(
            request.report_version,
            &request.session_id,
            request.connection_epoch.to_string(),
            generation.to_string(),
            request.reported_capture_unix.to_string(),
            &request.request_id,
            PROFILE,
            POLICY,
            PROBE,
            &data,
        ),
    )
    .map_err(|_| Error::Invalid)?;
    Ok(Checked {
        generation,
        data,
        digest,
        disposition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> IsolationObservation {
        IsolationObservation {
            report_version: 1,
            session_id: "session".into(),
            connection_epoch: 1,
            request_id: "inspection".into(),
            profile_id: PROFILE.into(),
            oci_policy_sha256: POLICY.into(),
            linux_probe_sha256: PROBE.into(),
            reported_capture_unix: 1000,
            node_policy_generation: Some(0),
            probe_json: include_bytes!(
                "../../../contracts/fixtures/isolation-v1/linux-x64-incomplete.json"
            )
            .to_vec(),
        }
    }

    #[test]
    fn pinned_release_keeps_real_missing_observations_separate_from_execution_authority() {
        let release: Value = serde_json::from_str(include_str!(
            "../../../config/worker-isolation-release.v1.json"
        ))
        .expect("release");
        assert_eq!(release["oci_policy_sha256"], POLICY);
        assert_eq!(release["linux_probe_sha256"], PROBE);
        assert_eq!(release["wasm_enabled"], false);
        assert_eq!(release["supply_authorized_by_report"], false);
        let mut input = request();
        let real = check(&input).expect("archived observed facts");
        assert_eq!(real.disposition, "incomplete");
        input.probe_json = serde_json::to_vec(&real.data).expect("compact");
        assert_eq!(
            check(&input).expect("same normalized bytes").digest,
            real.digest
        );
        input.connection_epoch += 1;
        assert_ne!(check(&input).expect("next session").digest, real.digest);
        let mut bad = real.data.clone();
        bad["disclose_paths"] = Value::Bool(true);
        input.probe_json = serde_json::to_vec(&bad).expect("negative");
        assert!(matches!(check(&input), Err(Error::Invalid)));
        input.probe_json=br#"{"protocol":1,"status":"observation_unavailable","runtime_verified":false,"supply_authorized":false}"#.to_vec();
        assert_eq!(
            check(&input).expect("no observations").disposition,
            "incomplete"
        );
        input.probe_json=br#"{"protocol":1,"status":"observation_unavailable","runtime_verified":true,"runtime_verified":false,"supply_authorized":false}"#.to_vec();
        assert!(matches!(check(&input), Err(Error::Invalid)));
    }
}
