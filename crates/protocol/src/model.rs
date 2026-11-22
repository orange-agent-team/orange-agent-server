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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
impl Message {
    pub fn plain(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
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
        let mut pending = std::collections::BTreeSet::new();
        let mut seen = std::collections::BTreeSet::new();
        for message in &self.messages {
            match message.role {
                Role::System | Role::User => {
                    if !pending.is_empty()
                        || !message.tool_calls.is_empty()
                        || message.tool_call_id.is_some()
                    {
                        return Err(ModelRequestError::Invalid);
                    }
                }
                Role::Assistant => {
                    if !pending.is_empty()
                        || message.tool_call_id.is_some()
                        || message.tool_calls.len() > 80
                    {
                        return Err(ModelRequestError::Invalid);
                    }
                    for call in &message.tool_calls {
                        if !crate::valid_id(&call.id)
                            || !crate::valid_id(&call.name)
                            || !call.arguments.is_object()
                            || !seen.insert(call.id.clone())
                            || !pending.insert(call.id.clone())
                        {
                            return Err(ModelRequestError::Invalid);
                        }
                    }
                }
                Role::Tool => {
                    if !message.tool_calls.is_empty()
                        || !message
                            .tool_call_id
                            .as_ref()
                            .is_some_and(|id| pending.remove(id))
                    {
                        return Err(ModelRequestError::Invalid);
                    }
                }
            }
        }
        if !pending.is_empty() {
            return Err(ModelRequestError::Invalid);
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

#[cfg(test)]
mod tests {
    use super::*;
    fn history() -> GenerateRequest {
        GenerateRequest {
            invocation_id: "inv".into(),
            model_version: "model".into(),
            max_output_tokens: 512,
            tools: vec![],
            messages: vec![
                Message::plain(Role::User, "read".into()),
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path":"note.txt"}),
                    }],
                    tool_call_id: None,
                },
                Message {
                    role: Role::Tool,
                    content: "untrusted result".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("c1".into()),
                },
            ],
        }
    }
    #[test]
    fn tool_history_requires_complete_unique_pairs() {
        let request = history();
        assert!(request.validate().is_ok());
        let mut missing = request.clone();
        missing.messages.pop();
        assert!(missing.validate().is_err());
        let mut wrong = request.clone();
        wrong.messages[2].tool_call_id = Some("other".into());
        assert!(wrong.validate().is_err());
        let mut spoof = request.clone();
        spoof.messages[2].role = Role::User;
        assert!(spoof.validate().is_err());
        let mut duplicate = request.clone();
        duplicate.messages.extend_from_slice(&request.messages[1..]);
        assert!(duplicate.validate().is_err());
        let mut interleaved = request;
        interleaved
            .messages
            .insert(2, Message::plain(Role::User, "override".into()));
        assert!(interleaved.validate().is_err());
    }
    #[test]
    fn dangerous_unknown_message_fields_and_context_overflow_fail() {
        assert!(serde_json::from_value::<Message>(serde_json::json!({"role":"tool","content":"ok","tool_call_id":"c1","approved":true})).is_err());
        let mut request = history();
        request.messages[0].content = "x".repeat(256 * 1024);
        assert_eq!(request.validate(), Err(ModelRequestError::Limit));
    }
}

/// Adapter observations contain only bounded categories. They never authorize replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayIssue {
    Unclassified,
    IdentityChanged,
    Invalid,
    Unavailable,
    Protocol,
    Limit,
    Incomplete,
    Cancelled,
}
impl GatewayIssue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::IdentityChanged => "identity_changed",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Protocol => "protocol",
            Self::Limit => "limit",
            Self::Incomplete => "incomplete",
            Self::Cancelled => "cancelled",
        }
    }
}
