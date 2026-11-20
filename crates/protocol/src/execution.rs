//! Capability1 canonical assignment and ACK identity. Transport authentication,
//! database authority and local monotonic expiry are separate mandatory checks.
use crate::{grpc::worker, request_digest, valid_digest, valid_id};
use serde_json::json;

fn positive(value: u64) -> bool {
    value > 0 && value <= i64::MAX as u64
}
pub fn output_header_digest(h: &worker::OutputHeader, a: &worker::Assignment) -> Option<String> {
    let ack = h.accepted.as_ref()?;
    if h.session_id != a.session_id
        || !valid_id(&h.upload_id)
        || !h
            .upload_id
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !valid_digest(&h.sha256)
        || !valid_digest(&h.response_digest)
        || !(1..=1024 * 1024).contains(&h.size_bytes)
        || ack.result != worker::AckResult::Accepted as i32
    {
        return None;
    }
    let accepted = acknowledge_digest(ack, a)?;
    request_digest("worker.output.header.v1",&json!({"session_id":h.session_id,"upload_id":h.upload_id,"accepted_digest":accepted,"sha256":h.sha256,"size_bytes":h.size_bytes.to_string(),"response_digest":h.response_digest})).ok()
}
pub fn renewal_digest(
    r: &worker::ExecutionRenewalRequest,
    a: &worker::Assignment,
) -> Option<String> {
    let ack = r.accepted.as_ref()?;
    if r.session_id != a.session_id
        || !valid_id(&r.request_id)
        || !valid_id(&r.supervisor_instance_id)
        || !positive(r.sequence)
        || ack.result != worker::AckResult::Accepted as i32
    {
        return None;
    }
    let accepted = acknowledge_digest(ack, a)?;
    request_digest(
        "worker.execution.renewal.v1",
        &json!({
            "session_id":r.session_id,"request_id":r.request_id,"sequence":r.sequence.to_string(),
            "accepted_digest":accepted,"supervisor_instance_id":r.supervisor_instance_id
        }),
    )
    .ok()
}
pub fn seconds(value: &Option<prost_types::Timestamp>) -> Option<i64> {
    value
        .as_ref()
        .filter(|v| (1..=253_402_300_799).contains(&v.seconds) && v.nanos == 0)
        .map(|v| v.seconds)
}
/// A report describes one approved CPU profile observed in the current session.
/// It is deliberately in a separate digest namespace from any lease or Job.
pub fn transcription_observation_digest(r: &worker::TranscriptionObservation) -> Option<String> {
    if r.profile_descriptor_json.len() > 16384 {
        return None;
    }
    let profile = crate::canonical::parse_tool_arguments(
        std::str::from_utf8(&r.profile_descriptor_json).ok()?,
    )
    .ok()?;
    if request_digest("worker.transcription-profile.v1", &profile).ok()?
        != r.execution_profile_digest
        || profile["device"] != "cpu"
        || profile["compute_type"] != r.compute_type
        || profile["model_sha256"] != r.model_sha256
        || profile["model_version"] != r.model_version
        || profile["preprocessing"] != "mono-16khz-s16-f32-v1"
        || profile["cpu_threads"] != 2
        || profile["workers"] != 1
        || profile["max_input_bytes"] != 100_000_000
        || profile["max_duration_ms"] != 600_000
        || !profile["runner_sha256"].as_str().is_some_and(valid_digest)
        || !profile["model_profile_sha256"]
            .as_str()
            .is_some_and(valid_digest)
    {
        return None;
    }
    let fixed: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../contracts/fixtures/transcription-observation.v1.json"
    ))
    .ok()?;
    if r.observation_version != 1
        || ![&r.session_id, &r.request_id, &r.execution_profile_id]
            .iter()
            .all(|s| valid_id(s))
        || !valid_digest(&r.execution_profile_digest)
        || r.model_version != fixed["model_version"]
        || r.model_sha256 != fixed["model_sha256"]
        || r.sample_sha256 != fixed["sample_sha256"]
        || r.transcript_sha256 != fixed["transcript_sha256"]
        || Some(r.duration_ms) != fixed["sample_duration_ms"].as_u64()
        || Some(r.normalized_samples) != fixed["normalized_samples"].as_u64()
        || !matches!(r.compute_type.as_str(), "int8" | "int8_float32")
    {
        return None;
    }
    request_digest("worker.transcription-observation.v1",&json!({
        "session_id":r.session_id,"request_id":r.request_id,"execution_profile_id":r.execution_profile_id,
        "execution_profile_digest":r.execution_profile_digest,"model_version":r.model_version,"model_sha256":r.model_sha256,
        "sample_sha256":r.sample_sha256,"duration_ms":r.duration_ms.to_string(),"normalized_samples":r.normalized_samples.to_string(),
        "transcript_sha256":r.transcript_sha256,"compute_type":r.compute_type
    })).ok()
}

pub fn assignment_digest(a: &worker::Assignment) -> Option<String> {
    let l = a.lease.as_ref()?;
    let expires = seconds(&l.expires_at)?;
    let deadline = seconds(&a.job_deadline)?;
    if ![
        &a.command_id,
        &a.node_id,
        &a.session_id,
        &l.lease_id,
        &l.job_id,
        &l.attempt_id,
        &a.model_version_id,
        &a.execution_profile_id,
    ]
    .iter()
    .all(|v| valid_id(v))
        || ![
            &a.model_sha256,
            &a.execution_profile_digest,
            &a.request_projection_digest,
        ]
        .iter()
        .all(|v| valid_digest(v))
        || !positive(l.connection_epoch)
        || !positive(l.fencing_token)
        || a.node_policy_generation > i64::MAX as u64
        || expires > deadline
        || a.input_artifact_ids.len() > 128
        || a.input_artifact_ids.iter().any(|v| !valid_id(v))
        || a.input_artifact_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != a.input_artifact_ids.len()
    {
        return None;
    }
    if !a.input_artifacts.is_empty() {
        if a.input_artifacts.len() != 1 || a.input_artifact_ids.len() != 1 {
            return None;
        }
        let input = &a.input_artifacts[0];
        if a.input_artifact_ids[0] != input.artifact_id
            || !valid_id(&input.artifact_id)
            || !valid_id(&input.version_id)
            || !valid_digest(&input.sha256)
            || input.size_bytes == 0
            || input.size_bytes > 256 * 1024
            || input.media_type != "application/json"
        {
            return None;
        }
    } else if !a.input_artifact_ids.is_empty() {
        return None;
    }
    let mut body = json!({
        "command_id": a.command_id, "node_id": a.node_id, "session_id": a.session_id,
        "policy_generation": a.node_policy_generation.to_string(),
        "lease_id": l.lease_id, "job_id": l.job_id, "attempt_id": l.attempt_id,
        "connection_epoch": l.connection_epoch.to_string(), "fencing_token": l.fencing_token.to_string(),
        "expires_at": expires.to_string(), "job_deadline": deadline.to_string(),
        "model_version_id": a.model_version_id, "model_sha256": a.model_sha256,
        "execution_profile_id": a.execution_profile_id, "execution_profile_digest": a.execution_profile_digest,
        "request_projection_digest": a.request_projection_digest, "input_artifact_ids": a.input_artifact_ids
    });
    // Preserve the exact digest of historical empty-input assignments.
    if !a.input_artifacts.is_empty() {
        body["input_versions"]=json!(a.input_artifacts.iter().map(|a|json!({"artifact_id":a.artifact_id,"version_id":a.version_id,"sha256":a.sha256,"size_bytes":a.size_bytes.to_string(),"media_type":a.media_type})).collect::<Vec<_>>());
    }
    request_digest("worker.assignment.v1", &body).ok()
}
pub fn acknowledge_digest(
    a: &worker::Acknowledge,
    expected: &worker::Assignment,
) -> Option<String> {
    if assignment_digest(expected)?.as_str() != expected.payload_digest
        || a.command_id != expected.command_id
        || a.payload_digest != expected.payload_digest
        || a.lease != expected.lease
    {
        return None;
    }
    match worker::AckResult::try_from(a.result).ok()? {
        worker::AckResult::Accepted => {
            if !valid_id(&a.execution_journal_id)
                || !valid_id(&a.broker_grant_id)
                || !positive(a.broker_epoch)
                || !positive(a.slot_generation)
            {
                return None;
            }
        }
        worker::AckResult::ResourceRejected | worker::AckResult::ProtocolRejected => {
            if !a.execution_journal_id.is_empty()
                || !a.broker_grant_id.is_empty()
                || a.broker_epoch != 0
                || a.slot_generation != 0
            {
                return None;
            }
        }
        worker::AckResult::AckUnspecified => return None,
    }
    request_digest("worker.ack.v1", &json!({"command_id":a.command_id,"payload_digest":a.payload_digest,
        "result":a.result, "execution_journal_id":a.execution_journal_id,"broker_grant_id":a.broker_grant_id,
        "broker_epoch":a.broker_epoch.to_string(),"slot_generation":a.slot_generation.to_string()})).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assignment() -> worker::Assignment {
        let mut a = worker::Assignment {
            command_id: "command".into(),
            payload_digest: String::new(),
            node_id: "node".into(),
            session_id: "session".into(),
            node_policy_generation: 9_007_199_254_740_993,
            lease: Some(worker::Lease {
                lease_id: "lease".into(),
                job_id: "job".into(),
                attempt_id: "attempt".into(),
                connection_epoch: 1,
                fencing_token: 9_007_199_254_740_993,
                expires_at: Some(prost_types::Timestamp {
                    seconds: 2_000_000_045,
                    nanos: 0,
                }),
            }),
            job_deadline: Some(prost_types::Timestamp {
                seconds: 2_000_000_100,
                nanos: 0,
            }),
            model_version_id: "model".into(),
            execution_profile_id: "profile".into(),
            model_sha256: "a".repeat(64),
            execution_profile_digest: "b".repeat(64),
            request_projection_digest: "c".repeat(64),
            input_artifact_ids: vec![],
            input_artifacts: vec![],
        };
        a.payload_digest = assignment_digest(&a).expect("valid");
        a
    }
    #[test]
    fn digest_preserves_large_fences_and_binds_model_and_session() {
        let a = assignment();
        for change in [0, 1, 2, 3, 4, 5] {
            let mut b = a.clone();
            match change {
                0 => b.node_policy_generation -= 1,
                1 => b.lease.as_mut().expect("lease").fencing_token -= 1,
                2 => b.model_sha256 = "d".repeat(64),
                3 => b.execution_profile_digest = "e".repeat(64),
                4 => b.session_id = "other-session".into(),
                _ => b.input_artifact_ids.push("other-input".into()),
            }
            assert_ne!(assignment_digest(&a), assignment_digest(&b));
        }
        let mut bad = a.clone();
        bad.lease.as_mut().expect("lease").connection_epoch = 0;
        assert!(assignment_digest(&bad).is_none());
        let mut bad = a.clone();
        bad.job_deadline.as_mut().expect("deadline").nanos = 1;
        assert!(assignment_digest(&bad).is_none());
        let mut bad = a.clone();
        bad.job_deadline.as_mut().expect("deadline").seconds = 253_402_300_800;
        assert!(assignment_digest(&bad).is_none());
        let mut bad = a;
        bad.input_artifact_ids = vec!["same".into(), "same".into()];
        assert!(assignment_digest(&bad).is_none());
    }
    #[test]
    fn accepted_ack_needs_exact_lease_and_local_journal_grant_identity() {
        let a = assignment();
        let ack = worker::Acknowledge {
            command_id: a.command_id.clone(),
            payload_digest: a.payload_digest.clone(),
            lease: a.lease.clone(),
            result: worker::AckResult::Accepted as i32,
            execution_journal_id: "journal".into(),
            broker_grant_id: "grant".into(),
            broker_epoch: 1,
            slot_generation: 1,
        };
        let original = acknowledge_digest(&ack, &a).expect("accepted");
        let mut bad = ack.clone();
        bad.lease.as_mut().expect("lease").fencing_token += 1;
        assert!(acknowledge_digest(&bad, &a).is_none());
        let mut bad = ack.clone();
        bad.execution_journal_id.clear();
        assert!(acknowledge_digest(&bad, &a).is_none());
        let mut bad = ack.clone();
        bad.result = 99;
        assert!(acknowledge_digest(&bad, &a).is_none());
        let mut changed = ack.clone();
        changed.slot_generation += 1;
        assert_ne!(
            acknowledge_digest(&changed, &a).expect("valid other identity"),
            original
        );
        let mut bad = ack;
        bad.result = worker::AckResult::ResourceRejected as i32;
        assert!(
            acknowledge_digest(&bad, &a).is_none(),
            "rejection cannot carry execution evidence"
        );
    }
    #[test]
    fn input_version_hash_and_media_are_bound_without_changing_legacy_digest() {
        let a = assignment();
        let legacy = assignment_digest(&a).expect("legacy");
        let mut b = a.clone();
        b.input_artifact_ids = vec!["input".into()];
        assert!(assignment_digest(&b).is_none());
        b.input_artifacts = vec![worker::InputArtifact {
            artifact_id: "input".into(),
            version_id: "version".into(),
            sha256: "e".repeat(64),
            size_bytes: 8,
            media_type: "application/json".into(),
        }];
        let digest = assignment_digest(&b).expect("versioned input");
        assert_ne!(legacy, digest);
        b.input_artifacts[0].version_id = "other-version".into();
        assert_ne!(assignment_digest(&b).expect("other"), digest);
        b.input_artifacts[0].size_bytes = 262145;
        assert!(assignment_digest(&b).is_none());
        assert_eq!(assignment_digest(&a).expect("unchanged legacy"), legacy);
    }
    #[test]
    fn renewal_identity_binds_large_sequence_supervisor_and_exact_accepted_ack() {
        let a = assignment();
        let mut r = worker::ExecutionRenewalRequest {
            session_id: a.session_id.clone(),
            request_id: "renewal".into(),
            sequence: 9_007_199_254_740_993,
            supervisor_instance_id: "supervisor".into(),
            accepted: Some(worker::Acknowledge {
                command_id: a.command_id.clone(),
                payload_digest: a.payload_digest.clone(),
                lease: a.lease.clone(),
                result: worker::AckResult::Accepted as i32,
                execution_journal_id: "journal".into(),
                broker_grant_id: "grant".into(),
                broker_epoch: 1,
                slot_generation: 1,
            }),
        };
        let first = renewal_digest(&r, &a).expect("canonical digest");
        r.sequence += 1;
        assert_ne!(renewal_digest(&r, &a).expect("next"), first);
        r.sequence -= 1;
        r.supervisor_instance_id = "another".into();
        assert_ne!(renewal_digest(&r, &a).expect("other"), first);
        r.session_id = "another-session".into();
        assert!(renewal_digest(&r, &a).is_none());
        r.session_id = a.session_id.clone();
        r.accepted
            .as_mut()
            .expect("ack")
            .lease
            .as_mut()
            .expect("lease")
            .fencing_token += 1;
        assert!(renewal_digest(&r, &a).is_none());
    }
    #[test]
    fn output_header_binds_exact_bytes_response_and_original_execution() {
        let a = assignment();
        let mut h = worker::OutputHeader {
            session_id: a.session_id.clone(),
            upload_id: "upload".into(),
            accepted: Some(worker::Acknowledge {
                command_id: a.command_id.clone(),
                payload_digest: a.payload_digest.clone(),
                lease: a.lease.clone(),
                result: worker::AckResult::Accepted as i32,
                execution_journal_id: "journal".into(),
                broker_grant_id: "grant".into(),
                broker_epoch: 1,
                slot_generation: 1,
            }),
            sha256: "a".repeat(64),
            size_bytes: 42,
            response_digest: "b".repeat(64),
        };
        let original = output_header_digest(&h, &a).expect("digest");
        h.sha256 = "c".repeat(64);
        assert_ne!(
            output_header_digest(&h, &a).expect("changed bytes"),
            original
        );
        h.sha256 = "a".repeat(64);
        h.response_digest = "d".repeat(64);
        assert_ne!(
            output_header_digest(&h, &a).expect("changed candidate"),
            original
        );
        h.size_bytes = 1048577;
        assert!(output_header_digest(&h, &a).is_none());
        h.size_bytes = 42;
        h.upload_id = "-invalid-upload".into();
        assert!(output_header_digest(&h, &a).is_none());
        h.upload_id = "upload".into();
        h.accepted
            .as_mut()
            .expect("ack")
            .lease
            .as_mut()
            .expect("lease")
            .fencing_token += 1;
        assert!(output_header_digest(&h, &a).is_none());
    }
}
