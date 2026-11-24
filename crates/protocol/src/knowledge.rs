//! Version-bound local knowledge tool inputs and outputs. File selection is host-only.
use serde::{Deserialize, Serialize};
pub const SCHEMA: &str = include_str!("../../../contracts/json-schema/knowledge-tools.v1.json");

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "tool",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum KnowledgeRequest {
    KnowledgeSearch(Search),
    KnowledgeRead(Read),
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub collection_id: String,
    pub query: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub collection_id: String,
    pub hit_id: String,
    pub expected_sha256: String,
    /// Offsets are relative to the cited UTF-8 chunk.
    pub start_byte: Option<usize>,
    pub end_byte: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    pub hit_id: String,
    pub path: String,
    pub source_sha256: String,
    pub start_byte: String,
    pub end_byte: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum KnowledgeResult {
    KnowledgeSearch {
        collection_id: String,
        hits: Vec<Hit>,
        truncated: bool,
        unavailable_sources: usize,
    },
    KnowledgeRead {
        citation: Hit,
        start_byte: String,
        end_byte: String,
        text: String,
    },
}
