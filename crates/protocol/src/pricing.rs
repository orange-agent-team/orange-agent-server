use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTextQuote {
    pub request_id: String,
    pub root_budget_id: String,
    pub price_version: String,
    pub input_upload_id: String,
    pub input_version_id: String,
    pub output_limit: u32,
}
impl CreateTextQuote {
    pub fn valid(&self) -> bool {
        [
            &self.request_id,
            &self.root_budget_id,
            &self.price_version,
            &self.input_upload_id,
            &self.input_version_id,
        ]
        .iter()
        .all(|s| crate::valid_id(s))
            && (1..=4096).contains(&self.output_limit)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextQuoteView {
    pub quote_id: String,
    pub root_budget_id: String,
    pub invocation_id: String,
    pub request_projection_digest: String,
    pub price_version: String,
    pub meter_profile_digest: String,
    pub input_units: String,
    pub output_limit: String,
    pub max_amount: crate::Money,
    pub expires_at_unix: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextPriceView {
    pub price_version: String,
    pub model_version: String,
    pub node_id: String,
    pub meter_profile_digest: String,
    pub input_price: crate::Money,
    pub output_price: crate::Money,
    pub output_limit: String,
    pub fee_bps: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasureAudio {
    pub version_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioMeasurement {
    pub artifact_id: String,
    pub version_id: String,
    pub audio_sha256: String,
    pub model_version: String,
    pub preprocessing: String,
    pub format: String,
    pub codec: String,
    pub source_sample_rate: u32,
    pub source_samples: String,
    pub normalized_samples: String,
    pub normalized_sha256: String,
    pub duration_ms: String,
}

/// Consent to send one measured immutable version to the observed node for one invocation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTranscriptionInputGrant {
    pub grant_id: String,
    pub root_budget_id: String,
    pub invocation_id: String,
    pub upload_id: String,
    pub observation_id: String,
    pub node_id: String,
    pub region_id: String,
    pub execution_profile_digest: String,
    pub language: Option<String>,
    pub ttl_seconds: u32,
    pub allow_remote_transcription: bool,
    pub measurement: AudioMeasurement,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionGrantReceipt {
    pub grant_id: String,
    pub invocation_id: String,
    pub observation_id: String,
    pub projection_digest: String,
    pub expires_at_unix: String,
    pub state: String,
}
pub const AUDIO_INPUT_CHUNK_BYTES: usize = 32_768;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionInputTarget {
    pub observation_id: String,
    pub node_id: String,
    pub region_id: String,
    pub model_version: String,
    pub execution_profile_digest: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionInputWithdrawal {
    pub grant_id: String,
    pub state: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionPrice {
    pub price_version: String,
    pub price_per_minute: crate::Money,
    pub fee_bps: u16,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTranscriptionQuote {
    pub request_id: String,
    pub grant_id: String,
    pub price_version: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionQuoteView {
    pub quote_id: String,
    pub grant_id: String,
    pub root_budget_id: String,
    pub invocation_id: String,
    pub request_projection_digest: String,
    pub price_version: String,
    pub observation_id: String,
    pub duration_ms: String,
    pub price_per_minute: crate::Money,
    pub max_amount: crate::Money,
    pub fee_bps: u16,
    pub expires_at_unix: String,
}

/// Point-in-time funding information for the original quote and its authenticated requester.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteFundingView {
    pub quote_id: String,
    pub requester_id: String,
    pub owner_id: String,
    pub root_budget_id: String,
    pub account_kind: FundingAccount,
    pub price_version: String,
    pub max_microcredits: String,
    pub budget_limit: String,
    pub budget_spent: String,
    pub budget_reserved: String,
    pub budget_remaining: String,
    pub wallet_available: Option<String>,
    pub status: FundingStatus,
    pub observed_unix: String,
    pub expires_unix: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingAccount {
    Personal,
    Team,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingStatus {
    Sufficient,
    Expired,
    BudgetClosed,
    SpendingPaused,
    InsufficientBudget,
    InsufficientFunds,
}
