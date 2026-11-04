//! Versioned host/model boundary; a candidate is never authority to execute tools.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRequestError {
    Invalid,
    Limit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerateRequest {
    pub invocation_id: String,
    pub model_version: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
}
impl GenerateRequest {
    pub fn validate(&self) -> Result<(), ModelRequestError> {
        if !crate::valid_id(&self.invocation_id)
            || !crate::valid_id(&self.model_version)
            || self.messages.is_empty()
            || self.messages.len() > 128
            || self.tools.len() > 80
            || !(1..=4096).contains(&self.max_output_tokens)
        {
            return Err(ModelRequestError::Invalid);
        }
        let mut names = std::collections::BTreeSet::new();
        for tool in &self.tools {
            if !crate::valid_id(&tool.name)
                || !names.insert(&tool.name)
                || !tool.parameters.is_object()
                || tool.description.len() > 4096
            {
                return Err(ModelRequestError::Invalid);
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| ModelRequestError::Invalid)?
            .len()
            > 256 * 1024
        {
            return Err(ModelRequestError::Limit);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseCandidate {
    pub response_id: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// Engine counters are evidence, never automatically the canonical billable units.
    pub native_usage: Option<NativeUsage>,
    pub finish_reason: String,
}
