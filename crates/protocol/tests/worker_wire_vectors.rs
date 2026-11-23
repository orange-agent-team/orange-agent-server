//! Read the fixed minimum/current bytes through the same prost types used by the Worker.
#![cfg(feature = "grpc")]

use orange_protocol::grpc::worker::{ServerFrame, WorkerFrame, server_frame, worker_frame};
use orange_protocol::worker_capabilities::{ControlVersions, NegotiationError};
use prost::Message;
use serde::Deserialize;

#[derive(Deserialize)]
struct Corpus {
    samples: Vec<Sample>,
}
#[derive(Deserialize)]
struct Sample {
    name: String,
    message: String,
    hex: String,
    expected: String,
}

#[test]
fn minimum_and_current_control_messages_preserve_authority_and_exact_epochs() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../../contracts/fixtures/worker-v1-baseline/vectors.json"
    ))
    .expect("wire samples");
    assert_eq!(corpus.samples.len(), 7);
    for sample in corpus.samples {
        let bytes = hex::decode(&sample.hex).expect("fixed wire bytes");
        let expectation = match sample.message.as_str() {
            "WorkerFrame" => {
                let frame = WorkerFrame::decode(bytes.as_slice()).expect("worker frame");
                assert_eq!(frame.encode_to_vec(), bytes);
                match frame.body.expect("known worker frame") {
                    worker_frame::Body::Hello(h) => {
                        assert_eq!(h.node_id, "legacy-node");
                        let versions = ControlVersions::from(&h);
                        match versions.validate(h.protocol_version) {
                            Ok(()) if versions == ControlVersions::default() => {
                                "accepted_without_execution"
                            }
                            Ok(()) => {
                                assert_eq!(versions.node_control_version, 2);
                                assert_eq!(versions.execution_control_version, 2);
                                "capabilities_declared"
                            }
                            Err(NegotiationError::Unsupported) => "FAILED_PRECONDITION",
                            Err(NegotiationError::MissingPrerequisite) => "INVALID_ARGUMENT",
                        }
                    }
                    worker_frame::Body::Heartbeat(h) => {
                        assert_eq!(h.connection_epoch, 9_007_199_254_740_993);
                        "liveness_only"
                    }
                    _ => panic!("unexpected control operation"),
                }
            }
            "ServerFrame" => {
                let frame = ServerFrame::decode(bytes.as_slice()).expect("server frame");
                assert_eq!(frame.encode_to_vec(), bytes);
                let Some(server_frame::Body::Welcome(w)) = frame.body else {
                    panic!("welcome required");
                };
                assert_eq!(w.connection_epoch, 9_007_199_254_740_993);
                assert_eq!(w.session_id, "session-fixture");
                let version = if sample.name == "floor-welcome" { 0 } else { 2 };
                assert!(
                    ControlVersions {
                        node_control_version: version,
                        execution_control_version: version,
                        ..Default::default()
                    }
                    .matches_welcome(&w)
                );
                "welcome"
            }
            _ => panic!("unrecognized fixture type"),
        };
        assert_eq!(expectation, sample.expected, "{}", sample.name);
    }
}
