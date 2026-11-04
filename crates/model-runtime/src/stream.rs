//! Bounded SSE and chat-delta assembly. No partial tool candidate escapes this module.
use crate::{ModelError, NativeUsage, ResponseCandidate, ToolCall};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    data: Vec<u8>,
    total: usize,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ModelError> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .ok_or(ModelError::Limit)?;
        if self.total > 8 * MAX_BYTES || self.pending.len() + bytes.len() > MAX_BYTES {
            return Err(ModelError::Limit);
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    self.data.pop();
                    events.push(
                        String::from_utf8(std::mem::take(&mut self.data))
                            .map_err(|_| ModelError::Protocol)?,
                    );
                }
            } else if let Some(value) = line.strip_prefix(b"data:") {
                let value = value.strip_prefix(b" ").unwrap_or(value);
                if self.data.len() + value.len() + 1 > MAX_BYTES {
                    return Err(ModelError::Limit);
                }
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
        }
        Ok(events)
    }
    pub fn finish(&self) -> Result<(), ModelError> {
        if !self.pending.is_empty() || !self.data.is_empty() {
            return Err(ModelError::Incomplete);
        }
        Ok(())
    }
}
#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}
#[derive(Default)]
pub struct ChatAssembler {
    response_id: Option<String>,
    text: String,
    tools: BTreeMap<u64, PendingTool>,
    usage: Option<NativeUsage>,
    finish: Option<String>,
    done: bool,
    bytes: usize,
    failed: bool,
}
impl ChatAssembler {
    pub fn event(&mut self, data: &str) -> Result<(), ModelError> {
        if self.failed {
            return Err(ModelError::Protocol);
        }
        let result = self.event_inner(data);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn event_inner(&mut self, data: &str) -> Result<(), ModelError> {
        if self.done {
            return Err(ModelError::Protocol);
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        self.bytes = self
            .bytes
            .checked_add(data.len())
            .ok_or(ModelError::Limit)?;
        if self.bytes > 8 * MAX_BYTES {
            return Err(ModelError::Limit);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
        if value.get("error").is_some() {
            return Err(ModelError::Protocol);
        }
        let id = value["id"]
            .as_str()
            .filter(|s| orange_protocol::valid_id(s))
            .ok_or(ModelError::Protocol)?;
        if self.response_id.as_deref().is_some_and(|s| s != id) {
            return Err(ModelError::Protocol);
        }
        self.response_id = Some(id.to_owned());
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            let next = NativeUsage {
                input_tokens: usage["prompt_tokens"]
                    .as_u64()
                    .ok_or(ModelError::Protocol)?,
                output_tokens: usage["completion_tokens"]
                    .as_u64()
                    .ok_or(ModelError::Protocol)?,
            };
            if self.usage.as_ref().is_some_and(|old| old != &next) {
                return Err(ModelError::Protocol);
            }
            self.usage = Some(next);
        }
        let choices = value["choices"].as_array().ok_or(ModelError::Protocol)?;
        if choices.is_empty() {
            if self.finish.is_none() {
                return Err(ModelError::Protocol);
            }
            return Ok(());
        }
        if choices.len() != 1 || choices[0]["index"].as_u64() != Some(0) || self.finish.is_some() {
            return Err(ModelError::Protocol);
        }
        let choice = &choices[0];
        let delta = choice["delta"].as_object().ok_or(ModelError::Protocol)?;
        // Unknown model-visible output variants cannot be silently discarded and committed.
        for key in delta.keys() {
            if !["role", "content", "tool_calls"].contains(&key.as_str()) && !delta[key].is_null() {
                return Err(ModelError::Protocol);
            }
        }
        if delta
            .get("role")
            .is_some_and(|v| v.as_str() != Some("assistant"))
        {
            return Err(ModelError::Protocol);
        }
        if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
            self.text
                .push_str(content.as_str().ok_or(ModelError::Protocol)?);
        }
        if let Some(tools) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            for item in tools.as_array().ok_or(ModelError::Protocol)? {
                let index = item["index"]
                    .as_u64()
                    .filter(|&n| n < 80)
                    .ok_or(ModelError::Protocol)?;
                let entry = self.tools.entry(index).or_default();
                if let Some(id) = item.get("id").filter(|v| !v.is_null()) {
                    let id = id
                        .as_str()
                        .filter(|s| orange_protocol::valid_id(s))
                        .ok_or(ModelError::Protocol)?;
                    if !entry.id.is_empty() && entry.id != id {
                        return Err(ModelError::Protocol);
                    }
                    entry.id = id.into();
                }
                if item
                    .get("type")
                    .is_some_and(|v| v.as_str() != Some("function"))
                {
                    return Err(ModelError::Protocol);
                }
                if let Some(function) = item.get("function") {
                    if let Some(name) = function.get("name") {
                        entry
                            .name
                            .push_str(name.as_str().ok_or(ModelError::Protocol)?);
                    }
                    if let Some(args) = function.get("arguments") {
                        entry
                            .arguments
                            .push_str(args.as_str().ok_or(ModelError::Protocol)?);
                    }
                }
                if entry.arguments.len() > MAX_BYTES || entry.name.len() > 128 {
                    return Err(ModelError::Limit);
                }
            }
        }
        if self.text.len() > MAX_BYTES {
            return Err(ModelError::Limit);
        }
        if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
            let reason = reason.as_str().ok_or(ModelError::Protocol)?;
            if !["stop", "tool_calls", "length"].contains(&reason) {
                return Err(ModelError::Protocol);
            }
            self.finish = Some(reason.to_owned());
        }
        Ok(())
    }
    pub fn finish(self) -> Result<ResponseCandidate, ModelError> {
        if self.failed || !self.done {
            return Err(ModelError::Incomplete);
        }
        let finish = self.finish.ok_or(ModelError::Incomplete)?;
        if finish == "length" {
            return Err(ModelError::Incomplete);
        }
        if (finish == "tool_calls") == self.tools.is_empty() {
            return Err(ModelError::Protocol);
        }
        let mut calls = Vec::new();
        let mut ids = BTreeSet::new();
        for (expected, (index, tool)) in self.tools.into_iter().enumerate() {
            if index != expected as u64
                || !orange_protocol::valid_id(&tool.id)
                || !orange_protocol::valid_id(&tool.name)
                || !ids.insert(tool.id.clone())
            {
                return Err(ModelError::Protocol);
            }
            let arguments: Value =
                serde_json::from_str(&tool.arguments).map_err(|_| ModelError::Incomplete)?;
            if !arguments.is_object() {
                return Err(ModelError::Protocol);
            }
            calls.push(ToolCall {
                id: tool.id,
                name: tool.name,
                arguments,
            });
        }
        Ok(ResponseCandidate {
            response_id: self.response_id.ok_or(ModelError::Incomplete)?,
            text: self.text,
            tool_calls: calls,
            native_usage: self.usage,
            finish_reason: finish,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn chunk(delta: Value, finish: Value) -> String {
        json!({"id":"response-1","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
            .to_string()
    }
    #[test]
    fn utf8_and_crlf_sse_are_independent_of_transport_chunks() {
        let data = format!(
            "data: {}\r\n\r\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(json!({"content":"你好"}), Value::Null),
            chunk(json!({}), json!("stop"))
        );
        for width in 1..data.len() {
            let mut decoder = SseDecoder::default();
            let mut assembler = ChatAssembler::default();
            for bytes in data.as_bytes().chunks(width) {
                for event in decoder.push(bytes).expect("SSE") {
                    assembler.event(&event).expect("event");
                }
            }
            decoder.finish().expect("EOF");
            assert_eq!(assembler.finish().expect("response").text, "你好");
        }
    }
    #[test]
    fn incomplete_or_truncated_tool_arguments_never_form_candidates() {
        for reason in ["tool_calls", "length"] {
            let mut assembler = ChatAssembler::default();
            assembler.event(&chunk(json!({"tool_calls":[{"index":0,"id":"call1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]}),json!(reason))).expect("partial event");
            assembler.event("[DONE]").expect("done");
            assert!(assembler.finish().is_err());
        }
    }
    #[test]
    fn complete_tools_wait_for_finish_and_transport_terminal() {
        let mut assembler = ChatAssembler::default();
        assembler.event(&chunk(json!({"tool_calls":[{"index":0,"id":"call1","function":{"name":"read_file","arguments":"{\"path\":"}}]}),Value::Null)).expect("first");
        assembler
            .event(&chunk(
                json!({"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]}),
                json!("tool_calls"),
            ))
            .expect("second");
        assembler.event("[DONE]").expect("done");
        assert_eq!(
            assembler.finish().expect("candidate").tool_calls[0].arguments["path"],
            "a"
        );
        let mut no_done = ChatAssembler::default();
        no_done
            .event(&chunk(json!({"content":"hello"}), json!("stop")))
            .expect("event");
        assert!(no_done.finish().is_err());
    }
    #[test]
    fn changed_identity_and_output_after_finish_fail_closed() {
        let mut assembler = ChatAssembler::default();
        assembler
            .event(&chunk(json!({"content":"a"}), json!("stop")))
            .expect("event");
        assert!(
            assembler
                .event(&chunk(json!({"content":"b"}), Value::Null))
                .is_err()
        );
        assert!(assembler.event("[DONE]").is_err());
        assert!(assembler.finish().is_err());
        let mut assembler = ChatAssembler::default();
        assembler
            .event(&chunk(json!({}), Value::Null))
            .expect("event");
        assert!(
            assembler
                .event(&chunk(json!({}), json!("stop")).replace("response-1", "response-2"))
                .is_err()
        );
    }
}
