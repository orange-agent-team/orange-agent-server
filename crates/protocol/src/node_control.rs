//! Stable node stop intent, separate from transport connection identity. A receipt
//! of this intent is not evidence that any process or device has been released.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopKind {
    #[serde(rename = "node.drain")]
    Drain,
    #[serde(rename = "node.quarantine")]
    Quarantine,
}
impl StopKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drain => "node.drain",
            Self::Quarantine => "node.quarantine",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeStopIntent {
    pub command_id: String,
    pub action_id: String,
    pub node_id: String,
    /// Positive canonical decimal, bounded by PostgreSQL BIGINT. JSON consumers
    /// never pass this fencing generation through an IEEE754 number.
    pub policy_generation: String,
    pub kind: StopKind,
}
impl NodeStopIntent {
    pub fn generation(&self) -> Option<i64> {
        let value = self.policy_generation.parse::<i64>().ok()?;
        (value > 0 && value.to_string() == self.policy_generation).then_some(value)
    }
    pub fn validate(&self) -> bool {
        [&self.command_id, &self.action_id, &self.node_id]
            .iter()
            .all(|value| {
                crate::valid_id(value)
                    && value
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
            })
            && self.generation().is_some()
    }
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        crate::request_digest("node-stop-intent-v1", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn intent() -> NodeStopIntent {
        NodeStopIntent {
            command_id: "command".into(),
            action_id: "action".into(),
            node_id: "node".into(),
            policy_generation: "9007199254740993".into(),
            kind: StopKind::Drain,
        }
    }
    #[test]
    fn stable_identity_binds_policy_and_preserves_wide_generation() {
        let original = intent();
        assert!(original.validate());
        assert_eq!(original.generation(), Some(9007199254740993));
        let body = serde_json::to_string(&original).expect("JSON");
        assert!(body.contains("\"9007199254740993\""));
        let roundtrip: NodeStopIntent = serde_json::from_str(&body).expect("decode");
        assert_eq!(roundtrip, original);
        let mut changed = original.clone();
        changed.kind = StopKind::Quarantine;
        assert_ne!(
            original.digest().expect("hash"),
            changed.digest().expect("hash")
        );
        changed = original.clone();
        changed.policy_generation = "9007199254740994".into();
        assert_ne!(
            original.digest().expect("hash"),
            changed.digest().expect("hash")
        );
    }
    #[test]
    fn dangerous_unknown_fields_and_noncanonical_generations_fail() {
        for value in ["0", "-1", "+1", "01", "1.0", "9223372036854775808"] {
            let mut command = intent();
            command.policy_generation = value.into();
            assert!(!command.validate());
        }
        let mut body = serde_json::to_value(intent()).expect("JSON");
        body["release_gpu"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NodeStopIntent>(body).is_err());
        let mut body = serde_json::to_value(intent()).expect("JSON");
        body["kind"] = serde_json::json!("node.resume");
        assert!(serde_json::from_value::<NodeStopIntent>(body).is_err());
    }
}
