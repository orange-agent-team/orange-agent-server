pub mod canonical;
pub mod events;
pub mod model;
pub mod team;

use orange_domain::Credits;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;

/// JSON money is a canonical decimal string, never a floating point number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Money(pub Credits);
impl TryFrom<String> for Money {
    type Error = orange_domain::DomainError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Credits::from_str(&value).map(Self)
    }
}
impl From<Money> for String {
    fn from(value: Money) -> Self {
        value.0.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBudget {
    pub scope_id: String,
    pub limit: Money,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetView {
    pub root_budget_id: String,
    pub scope_id: String,
    pub limit: Money,
    pub spent: Money,
    pub reserved: Money,
    pub status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitJob {
    pub root_budget_id: String,
    pub invocation_id: String,
    pub purpose: InvocationPurpose,
    pub quote_id: String,
    pub request_projection_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvocationPurpose {
    Primary,
    Summary,
    Memory,
    Title,
    Continuation,
    Embedding,
    Image,
    Transcription,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobView {
    pub job_id: String,
    pub root_budget_id: String,
    pub invocation_id: String,
    pub execution_state: String,
    pub settlement_state: String,
    pub reserved: Money,
    pub charge: Option<Money>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub request_id: String,
    pub retry_class: String,
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}
pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Stable object-key ordering; only typed Orange DTOs (not arbitrary JSON numbers)
/// enter business idempotency. This is versioned Orange JSON, not a claim of JCS.
pub fn request_digest<T: Serialize>(
    operation: &str,
    value: &T,
) -> Result<String, serde_json::Error> {
    let body = serde_json::to_value(value)?;
    let canonical = serde_json::to_vec(&body)?;
    let mut hash = Sha256::new();
    hash.update(b"orange-request-v1\0");
    hash.update(operation.as_bytes());
    hash.update([0]);
    hash.update(canonical);
    Ok(hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn money_roundtrips_without_javascript_precision_loss() {
        let text = "\"9223372036854775807\"";
        let money: Money = serde_json::from_str(text).expect("money");
        assert_eq!(serde_json::to_string(&money).expect("serialize"), text);
        for bad in [
            "1",
            "1.1",
            "\"01\"",
            "\"1e9\"",
            "\"-1\"",
            "\"9223372036854775808\"",
        ] {
            assert!(serde_json::from_str::<Money>(bad).is_err());
        }
    }
    #[test]
    fn unknown_fields_and_purposes_fail_closed() {
        assert!(
            serde_json::from_str::<CreateBudget>(
                r#"{"scope_id":"run","limit":"1","actor":"root"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<InvocationPurpose>("\"free_background_call\"").is_err());
    }
    #[test]
    fn digest_binds_operation_and_content_not_json_key_order() {
        let a: CreateBudget = serde_json::from_str(r#"{"scope_id":"run","limit":"1"}"#).expect("a");
        let b: CreateBudget = serde_json::from_str(r#"{"limit":"1","scope_id":"run"}"#).expect("b");
        assert_eq!(
            request_digest("create_budget", &a).expect("digest"),
            request_digest("create_budget", &b).expect("digest")
        );
        assert_ne!(
            request_digest("create_budget", &a).expect("digest"),
            request_digest("other", &a).expect("digest")
        );
        assert!(valid_digest(
            &request_digest("create_budget", &a).expect("digest")
        ));
        assert!(!valid_id("../other-owner"));
    }
}
