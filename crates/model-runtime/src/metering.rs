//! DR-01 candidate profile. This projects already validated output; it does not authorize tools or billing.
use orange_protocol::{canonical::canonical_tool_arguments, request_digest, valid_id};
use serde::{Deserialize, Serialize};
pub const PROFILE: &str = "orange-visible-components-v1";
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawToolOutput {
    pub name: String,
    pub arguments_json: String,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    Text,
    ToolName,
    ToolArguments,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutputPart {
    pub kind: PartKind,
    pub tool_index: Option<u16>,
    pub text: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct OutputProjection {
    profile_id: &'static str,
    parts: Vec<OutputPart>,
    projection_digest: String,
    has_tools: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionError {
    InvalidArguments,
    InvalidTool,
    Limit,
}
impl OutputProjection {
    pub fn parts(&self) -> &[OutputPart] {
        &self.parts
    }
    pub fn has_tools(&self) -> bool {
        self.has_tools
    }
    pub fn digest(&self) -> &str {
        &self.projection_digest
    }
}
pub fn project(text: &str, tools: &[RawToolOutput]) -> Result<OutputProjection, ProjectionError> {
    if tools.len() > 80 || text.len() > 1024 * 1024 {
        return Err(ProjectionError::Limit);
    }
    let mut parts = vec![OutputPart {
        kind: PartKind::Text,
        tool_index: None,
        text: text.into(),
    }];
    let mut bytes = text.len();
    for (index, tool) in tools.iter().enumerate() {
        if !valid_id(&tool.name) {
            return Err(ProjectionError::InvalidTool);
        }
        let arguments = canonical_tool_arguments(&tool.arguments_json)
            .map_err(|_| ProjectionError::InvalidArguments)?;
        bytes = bytes
            .checked_add(tool.name.len() + arguments.len())
            .ok_or(ProjectionError::Limit)?;
        if bytes > 1024 * 1024 {
            return Err(ProjectionError::Limit);
        }
        let index = Some(u16::try_from(index).map_err(|_| ProjectionError::Limit)?);
        parts.push(OutputPart {
            kind: PartKind::ToolName,
            tool_index: index,
            text: tool.name.clone(),
        });
        parts.push(OutputPart {
            kind: PartKind::ToolArguments,
            tool_index: index,
            text: String::from_utf8(arguments).map_err(|_| ProjectionError::InvalidArguments)?,
        });
    }
    let projection_digest =
        request_digest(PROFILE, &parts).map_err(|_| ProjectionError::InvalidArguments)?;
    Ok(OutputProjection {
        profile_id: PROFILE,
        parts,
        projection_digest,
        has_tools: !tools.is_empty(),
    })
}

pub fn project_candidate(
    candidate: &orange_protocol::model::ResponseCandidate,
) -> Result<OutputProjection, ProjectionError> {
    if (candidate.finish_reason == "stop" && !candidate.tool_calls.is_empty())
        || (candidate.finish_reason == "tool_calls" && candidate.tool_calls.is_empty())
        || !["stop", "tool_calls"].contains(&candidate.finish_reason.as_str())
    {
        return Err(ProjectionError::InvalidArguments);
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut tools = Vec::new();
    for call in &candidate.tool_calls {
        if !valid_id(&call.id) || !ids.insert(&call.id) {
            return Err(ProjectionError::InvalidTool);
        }
        tools.push(RawToolOutput {
            name: call.name.clone(),
            arguments_json: serde_json::to_string(&call.arguments)
                .map_err(|_| ProjectionError::InvalidArguments)?,
        });
    }
    project(&candidate.text, &tools)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_identity_and_native_usage_are_not_billable_output() {
        use orange_protocol::model::{NativeUsage, ResponseCandidate, ToolCall};
        let mut candidate = ResponseCandidate {
            response_id: "response-a".into(),
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-a".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md"}),
            }],
            native_usage: Some(NativeUsage {
                input_tokens: 100,
                output_tokens: 50,
            }),
            finish_reason: "tool_calls".into(),
        };
        let a = project_candidate(&candidate).expect("projection");
        candidate.response_id = "response-b".into();
        candidate.tool_calls[0].id = "call-b".into();
        candidate.native_usage = Some(NativeUsage {
            input_tokens: 200,
            output_tokens: 90,
        });
        let b = project_candidate(&candidate).expect("same visible components");
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.parts(), b.parts());
    }
    #[test]
    fn transport_fragmentation_and_argument_key_order_do_not_change_projection() {
        let a = project(
            "hello",
            &[RawToolOutput {
                name: "read_file".into(),
                arguments_json: "{\"path\":\"a\",\"offset\":1.0}".into(),
            }],
        )
        .expect("projection");
        let b = project(
            &["he", "l", "lo"].concat(),
            &[RawToolOutput {
                name: "read_file".into(),
                arguments_json: ["{\"offset\":", "1,\"path\":\"a\"}"].concat(),
            }],
        )
        .expect("projection");
        assert_eq!(a.parts(), b.parts());
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.parts()[2].text, "{\"offset\":1,\"path\":\"a\"}");
    }
}
