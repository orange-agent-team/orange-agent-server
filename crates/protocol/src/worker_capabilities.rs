//! Worker v1 capability negotiation, shared by the Gateway, session store and Worker client.
//! A valid offer is neither device authorization nor an execution grant.
use crate::grpc::worker::{Hello, Welcome};

pub const PROTOCOL_MAJOR: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControlVersions {
    pub node_control_version: u32,
    pub execution_control_version: u32,
    pub rejection_receipts_version: u32,
    pub input_artifacts_version: u32,
    pub transcription_input_version: u32,
    pub transcription_output_version: u32,
    pub renewal_version: u32,
    pub output_version: u32,
    pub cpu_cleanup_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationError {
    Unsupported,
    MissingPrerequisite,
}

impl ControlVersions {
    pub fn validate(self, major: u32) -> Result<(), NegotiationError> {
        let bounded = [
            self.rejection_receipts_version,
            self.input_artifacts_version,
            self.transcription_input_version,
            self.transcription_output_version,
            self.renewal_version,
            self.output_version,
            self.cpu_cleanup_version,
        ];
        if major != PROTOCOL_MAJOR
            || self.node_control_version > 2
            || self.execution_control_version > 2
            || bounded.iter().any(|v| *v > 1)
            || (self.transcription_input_version == 1 && self.input_artifacts_version != 1)
            || (self.transcription_output_version == 1
                && (self.transcription_input_version != 1 || self.output_version != 1))
        {
            return Err(NegotiationError::Unsupported);
        }
        let needs_current_authority = self.cpu_cleanup_version == 1
            || self.input_artifacts_version == 1
            || self.rejection_receipts_version == 1;
        if (needs_current_authority
            && (self.node_control_version != 2 || self.execution_control_version != 2))
            || (self.output_version == 1
                && (self.renewal_version != 1 || self.input_artifacts_version != 1))
            || (self.renewal_version == 1 && self.input_artifacts_version != 1)
        {
            return Err(NegotiationError::MissingPrerequisite);
        }
        Ok(())
    }

    pub fn hello(self, node_id: String, capabilities_digest: String) -> Hello {
        Hello {
            node_id,
            capabilities_digest,
            protocol_version: PROTOCOL_MAJOR,
            node_control_version: self.node_control_version,
            execution_control_version: self.execution_control_version,
            rejection_receipts_version: self.rejection_receipts_version,
            input_artifacts_version: self.input_artifacts_version,
            transcription_input_version: self.transcription_input_version,
            transcription_output_version: self.transcription_output_version,
            renewal_version: self.renewal_version,
            output_version: self.output_version,
            cpu_cleanup_version: self.cpu_cleanup_version,
        }
    }
    pub fn matches_welcome(self, w: &Welcome) -> bool {
        w.protocol_version == PROTOCOL_MAJOR
            && self.validate(w.protocol_version).is_ok()
            && self.node_control_version == w.node_control_version
            && self.execution_control_version == w.execution_control_version
            && self.rejection_receipts_version == w.rejection_receipts_version
            && self.input_artifacts_version == w.input_artifacts_version
            && self.transcription_input_version == w.transcription_input_version
            && self.transcription_output_version == w.transcription_output_version
            && self.renewal_version == w.renewal_version
            && self.output_version == w.output_version
            && self.cpu_cleanup_version == w.cpu_cleanup_version
    }
}

impl From<&Hello> for ControlVersions {
    fn from(h: &Hello) -> Self {
        Self {
            node_control_version: h.node_control_version,
            execution_control_version: h.execution_control_version,
            rejection_receipts_version: h.rejection_receipts_version,
            input_artifacts_version: h.input_artifacts_version,
            transcription_input_version: h.transcription_input_version,
            transcription_output_version: h.transcription_output_version,
            renewal_version: h.renewal_version,
            output_version: h.output_version,
            cpu_cleanup_version: h.cpu_cleanup_version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_v1_capability_combination_has_consistent_provider_and_gateway_semantics() {
        let mut accepted = 0;
        for node in 0..=2 {
            for execution in 0..=2 {
                for mask in 0u32..128 {
                    let bit = |b: u32| (mask >> b) & 1;
                    let v = ControlVersions {
                        node_control_version: node,
                        execution_control_version: execution,
                        rejection_receipts_version: bit(0),
                        input_artifacts_version: bit(1),
                        transcription_input_version: bit(2),
                        transcription_output_version: bit(3),
                        renewal_version: bit(4),
                        output_version: bit(5),
                        cpu_cleanup_version: bit(6),
                    };
                    assert_eq!(
                        ControlVersions::from(&v.hello("node".into(), "a".repeat(64))),
                        v
                    );
                    let current = node == 2 && execution == 2;
                    let valid = (bit(0) == 0 || current)
                        && (bit(1) == 0 || current)
                        && (bit(2) == 0 || bit(1) == 1)
                        && (bit(3) == 0 || (bit(2) == 1 && bit(5) == 1))
                        && (bit(4) == 0 || bit(1) == 1)
                        && (bit(5) == 0 || (bit(1) == 1 && bit(4) == 1))
                        && (bit(6) == 0 || current);
                    assert_eq!(v.validate(1).is_ok(), valid);
                    if valid {
                        accepted += 1;
                    }
                }
            }
        }
        assert!(accepted > 9 && accepted < 1152);
    }
    #[test]
    fn future_major_and_silent_capability_changes_are_rejected() {
        let v = ControlVersions::default();
        let mut w = Welcome {
            protocol_version: 1,
            ..Default::default()
        };
        assert!(v.matches_welcome(&w));
        w.output_version = 1;
        assert!(!v.matches_welcome(&w));
        assert_eq!(v.validate(2), Err(NegotiationError::Unsupported));
        assert_eq!(
            ControlVersions {
                renewal_version: 1,
                ..v
            }
            .validate(1),
            Err(NegotiationError::MissingPrerequisite)
        );
        assert_eq!(
            ControlVersions {
                cpu_cleanup_version: 2,
                ..v
            }
            .validate(1),
            Err(NegotiationError::Unsupported)
        );
    }
}
