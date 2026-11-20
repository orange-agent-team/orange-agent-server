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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
