use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventStreamView {
    pub stream_id: String,
    pub entity_kind: String,
    pub entity_id: String,
    pub next_seq: String,
    pub oldest_seq: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub stream_id: String,
    pub seq: String,
    pub owner_id: String,
    pub entity_kind: String,
    pub entity_id: String,
    pub event_type: String,
    pub occurred_at: String,
    pub payload_digest: String,
    pub correlation_id: String,
    pub causation_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    pub stream_id: String,
    pub events: Vec<DurableEvent>,
    pub next_cursor: String,
}
/// Decimal cursor stays lossless across JavaScript and Rust. Zero denotes before the first event.
pub fn parse_cursor(value: &str) -> Option<i64> {
    if value.is_empty()
        || value.len() > 19
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}
