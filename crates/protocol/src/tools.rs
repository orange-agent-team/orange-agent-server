use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    pub path: String,
    pub start_byte: Option<u64>,
    pub max_bytes: Option<u32>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub expected_sha256: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArgs {
    pub path: String,
    pub cursor: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchFilesArgs {
    pub path: String,
    pub glob: String,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchFilesResult {
    pub path: String,
    pub paths: Vec<String>,
    pub scanned_entries: u32,
    pub truncated: bool,
    pub omitted_unsupported: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTextArgs {
    pub path: String,
    pub query: String,
    pub glob: Option<String>,
    pub limit: Option<u16>,
    pub regular_expression: Option<bool>,
    pub case_sensitive: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextMatch {
    pub path: String,
    pub sha256: String,
    pub line: String,
    pub start_byte: String,
    pub end_byte: String,
    pub excerpt: String,
    pub excerpt_truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTextResult {
    pub path: String,
    pub matches: Vec<TextMatch>,
    pub scanned_files: u16,
    pub scanned_bytes: String,
    pub truncated: bool,
    pub omitted_unsupported: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileResult {
    pub path: String,
    pub sha256: String,
    pub total_bytes: String,
    pub start_byte: String,
    pub end_byte: String,
    pub content: String,
    pub truncated: bool,
    pub next_byte: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: String,
    pub bytes: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryResult {
    pub path: String,
    pub snapshot_digest: String,
    pub entries: Vec<DirectoryEntry>,
    pub next_cursor: Option<String>,
    pub omitted_unsupported: bool,
}
