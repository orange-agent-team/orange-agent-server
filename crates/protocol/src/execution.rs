//! Capability1 canonical assignment and ACK identity. Transport authentication,
//! database authority and local monotonic expiry are separate mandatory checks.
use crate::{grpc::worker, request_digest, valid_digest, valid_id};
use serde_json::json;

fn positive(value: u64) -> bool {
    value > 0 && value <= i64::MAX as u64
}
pub fn seconds(value: &Option<prost_types::Timestamp>) -> Option<i64> {
    value
        .as_ref()
        .filter(|v| (1..=253_402_300_799).contains(&v.seconds) && v.nanos == 0)
        .map(|v| v.seconds)
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
    request_digest("worker.assignment.v1", &json!({
        "command_id": a.command_id, "node_id": a.node_id, "session_id": a.session_id,
        "policy_generation": a.node_policy_generation.to_string(),
        "lease_id": l.lease_id, "job_id": l.job_id, "attempt_id": l.attempt_id,
        "connection_epoch": l.connection_epoch.to_string(), "fencing_token": l.fencing_token.to_string(),
        "expires_at": expires.to_string(), "job_deadline": deadline.to_string(),
        "model_version_id": a.model_version_id, "model_sha256": a.model_sha256,
        "execution_profile_id": a.execution_profile_id, "execution_profile_digest": a.execution_profile_digest,
        "request_projection_digest": a.request_projection_digest, "input_artifact_ids": a.input_artifact_ids
    })).ok()
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
}
