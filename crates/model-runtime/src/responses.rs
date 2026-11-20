//! Stateless Responses input and streaming output for the fixed local text backend.
use crate::{GenerateRequest, ModelError, NativeUsage, ResponseCandidate, Role, ToolCall};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn request_body(request: &GenerateRequest, stream: bool) -> Value {
    let mut input = Vec::new();
    for message in &request.messages {
        if message.role == Role::Tool {
            input.push(json!({"type":"function_call_output","call_id":message.tool_call_id,"output":message.content}));
            continue;
        }
        if !message.content.is_empty() || message.tool_calls.is_empty() {
            input.push(json!({"type":"message","role":message.role,"content":message.content}));
        }
        for call in &message.tool_calls {
            input.push(json!({"type":"function_call","name":call.name,"call_id":call.id,"arguments":call.arguments.to_string()}));
        }
    }
    let tools: Vec<_> = request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect();
    json!({"model":request.model_version,"input":input,"tools":tools,"store":false,
          "stream":stream,"max_output_tokens":request.max_output_tokens,"temperature":0})
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    value[key].as_str().ok_or(ModelError::Protocol)
}
fn id<'a>(value: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    let id = field(value, key)?;
    orange_protocol::valid_id(id)
        .then_some(id)
        .ok_or(ModelError::Protocol)
}
fn require(condition: bool) -> Result<(), ModelError> {
    condition.then_some(()).ok_or(ModelError::Protocol)
}
struct Item {
    start: Value,
    text: String,
    part_started: bool,
    text_done: bool,
    part_done: bool,
    finished: Option<Value>,
}
impl Item {
    fn new(start: &Value) -> Result<Self, ModelError> {
        id(start, "id")?;
        require(start["status"] == "in_progress")?;
        match field(start, "type")? {
            "message" => require(start["role"] == "assistant" && start["content"] == json!([]))?,
            "function_call" => {
                id(start, "call_id")?;
                id(start, "name")?;
                require(start["arguments"] == "")?;
            }
            _ => return Err(ModelError::Protocol),
        }
        Ok(Self {
            start: start.clone(),
            text: String::new(),
            part_started: false,
            text_done: false,
            part_done: false,
            finished: None,
        })
    }
    fn event(&mut self, kind: &str, value: &Value) -> Result<(), ModelError> {
        require(self.finished.is_none())?;
        let message = self.start["type"] == "message";
        if kind == "response.output_item.done" {
            let end = &value["item"];
            require(
                end["id"] == self.start["id"]
                    && end["type"] == self.start["type"]
                    && end["status"] == "completed",
            )?;
            if message {
                let parts = end["content"].as_array().ok_or(ModelError::Protocol)?;
                require(
                    self.part_done
                        && end["role"] == "assistant"
                        && parts.len() == 1
                        && parts[0]["type"] == "output_text"
                        && parts[0]["text"] == self.text,
                )?;
            } else {
                require(
                    end["name"] == self.start["name"]
                        && end["call_id"] == self.start["call_id"]
                        && end["arguments"] == self.text,
                )?;
            }
            self.finished = Some(end.clone());
            return Ok(());
        }
        require(value["item_id"] == self.start["id"])?;
        if message {
            require(value["content_index"].as_u64() == Some(0))?;
        }
        match kind {
            "response.content_part.added" if message && !self.part_started => {
                require(value["part"]["type"] == "output_text" && value["part"]["text"] == "")?;
                self.part_started = true;
            }
            "response.output_text.delta" if message && self.part_started && !self.text_done => {
                self.text.push_str(field(value, "delta")?)
            }
            "response.function_call_arguments.delta" if !message && !self.text_done => {
                self.text.push_str(field(value, "delta")?)
            }
            "response.output_text.done" if message && self.part_started && !self.text_done => {
                require(value["text"] == self.text)?;
                self.text_done = true;
            }
            "response.function_call_arguments.done" if !message && !self.text_done => {
                require(value["arguments"] == self.text)?;
                self.text_done = true;
            }
            "response.content_part.done" if message && self.text_done && !self.part_done => {
                require(
                    value["part"]["type"] == "output_text" && value["part"]["text"] == self.text,
                )?;
                self.part_done = true;
            }
            _ => return Err(ModelError::Protocol),
        }
        if self.text.len() > 1024 * 1024 {
            return Err(ModelError::Limit);
        }
        Ok(())
    }
}
#[derive(Default)]
pub(crate) struct ResponsesAssembler {
    response_id: Option<String>,
    items: Vec<Item>,
    completed: Option<Value>,
    sequence: Option<u64>,
    closed: bool,
}
impl ResponsesAssembler {
    // The transport drops this entire assembler on the first failed event.
    pub(crate) fn event(&mut self, data: &str) -> Result<(), ModelError> {
        require(!self.closed)?;
        if data == "[DONE]" {
            require(self.completed.is_some())?;
            self.closed = true;
            return Ok(());
        }
        require(self.completed.is_none())?;
        let value: Value = serde_json::from_str(data).map_err(|_| ModelError::Protocol)?;
        if value.get("sequence_number").is_some() || self.sequence.is_some() {
            let seq = value["sequence_number"]
                .as_u64()
                .ok_or(ModelError::Protocol)?;
            require(self.sequence.map_or(self.response_id.is_none(), |old| {
                old.checked_add(1) == Some(seq)
            }))?;
            self.sequence = Some(seq);
        }
        let kind = field(&value, "type")?;
        let response = &value["response"];
        if kind == "response.created" {
            require(
                self.response_id.is_none()
                    && response["object"] == "response"
                    && response["status"] == "in_progress",
            )?;
            self.response_id = Some(id(response, "id")?.to_owned());
            return Ok(());
        }
        let response_id = self.response_id.as_deref().ok_or(ModelError::Protocol)?;
        match kind {
            "response.in_progress" => {
                require(response["id"] == response_id && response["status"] == "in_progress")
            }
            "response.incomplete" | "response.failed" | "error" => Err(ModelError::Incomplete),
            "response.completed" => {
                require(
                    response["id"] == response_id
                        && response["object"] == "response"
                        && response["status"] == "completed"
                        && response["error"].is_null()
                        && response["incomplete_details"].is_null(),
                )?;
                let output = response["output"].as_array().ok_or(ModelError::Protocol)?;
                require(
                    output.len() == self.items.len()
                        && !output.is_empty()
                        && output
                            .iter()
                            .zip(&self.items)
                            .all(|(end, item)| item.finished.as_ref() == Some(end)),
                )?;
                self.completed = Some(response.clone());
                Ok(())
            }
            "response.output_item.added" => {
                let start = &value["item"];
                require(
                    value["output_index"].as_u64() == Some(self.items.len() as u64)
                        && self.items.len() < 81
                        && !self
                            .items
                            .iter()
                            .any(|other| other.start["id"] == start["id"]),
                )?;
                self.items.push(Item::new(start)?);
                Ok(())
            }
            _ => {
                let index = value["output_index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or(ModelError::Protocol)?;
                self.items
                    .get_mut(index)
                    .ok_or(ModelError::Protocol)?
                    .event(kind, &value)
            }
        }
    }
    pub(crate) fn finish(
        self,
        model: &str,
        max_output: u32,
    ) -> Result<ResponseCandidate, ModelError> {
        let response = self.completed.ok_or(ModelError::Incomplete)?;
        require(response["model"] == model)?;
        let usage = &response["usage"];
        let native = NativeUsage {
            input_tokens: usage["input_tokens"].as_u64().ok_or(ModelError::Protocol)?,
            output_tokens: usage["output_tokens"]
                .as_u64()
                .ok_or(ModelError::Protocol)?,
        };
        require(
            native.input_tokens.checked_add(native.output_tokens)
                == Some(usage["total_tokens"].as_u64().ok_or(ModelError::Protocol)?),
        )?;
        // This pinned backend reports completed even at its token cap. A cut-off tool is unsafe.
        if native.output_tokens >= u64::from(max_output) {
            return Err(ModelError::Incomplete);
        }
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut ids = BTreeSet::new();
        for item in self.items {
            if item.start["type"] == "message" {
                text.push_str(&item.text);
            } else {
                let call = id(&item.start, "call_id")?.to_owned();
                require(ids.insert(call.clone()) && calls.len() < 80)?;
                let args = orange_protocol::canonical::parse_tool_arguments(&item.text)
                    .map_err(|_| ModelError::Protocol)?;
                require(args.is_object())?;
                calls.push(ToolCall {
                    id: call,
                    name: field(&item.start, "name")?.into(),
                    arguments: args,
                });
            }
        }
        Ok(ResponseCandidate {
            response_id: self.response_id.ok_or(ModelError::Incomplete)?,
            text,
            finish_reason: if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
            tool_calls: calls,
            native_usage: Some(native),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> Vec<Value> {
        let pending = json!({"id":"fc_1","type":"function_call","name":"read_file","call_id":"call_1","arguments":"","status":"in_progress"});
        let mut done = pending.clone();
        done["arguments"] = json!("{\"path\":\"文件\"}");
        done["status"] = json!("completed");
        vec![
            json!({"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":pending}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"\"文件\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":done}),
            json!({"type":"response.completed","response":{"id":"resp_1","object":"response","model":"test","status":"completed","usage":{"input_tokens":12,"output_tokens":5,"total_tokens":17},"output":[done]}}),
        ]
    }

    fn assemble(events: &[Value], max_output: u32) -> Result<ResponseCandidate, ModelError> {
        let mut assembler = ResponsesAssembler::default();
        for event in events {
            assembler.event(&event.to_string())?;
        }
        assembler.finish("test", max_output)
    }

    #[test]
    fn no_prefix_authorizes_a_candidate() {
        let events = stream();
        for prefix in 0..events.len() {
            assert!(assemble(&events[..prefix], 6).is_err());
        }
        assert!(matches!(assemble(&events, 5), Err(ModelError::Incomplete)));
        let reply = assemble(&events, 6).expect("complete and under budget");
        assert_eq!(reply.tool_calls[0].id, "call_1");
        assert_eq!(reply.tool_calls[0].arguments["path"], "文件");
        assert_eq!(reply.native_usage.expect("usage").output_tokens, 5);
    }

    #[test]
    fn reject_inconsistent_snapshots_identities_and_usage() {
        for (index, pointer, replacement) in [
            (2, "/item_id", json!("fc_other")),
            (2, "/output_index", json!(2)),
            (3, "/delta", json!("null}")),
            (4, "/item/name", json!("other_tool")),
            (4, "/item/call_id", json!("call_other")),
            (5, "/response/id", json!("resp_other")),
            (5, "/response/status", json!("incomplete")),
            (5, "/response/output/0/arguments", json!("{}")),
            (5, "/response/usage/total_tokens", json!(9)),
            (5, "/response/model", json!("different")),
        ] {
            let mut events = stream();
            *events[index].pointer_mut(pointer).expect("fixture") = replacement;
            assert!(assemble(&events, 10).is_err(), "{pointer}");
        }
        let mut events = stream();
        events.push(events[5].clone());
        assert!(assemble(&events, 10).is_err());
    }

    #[test]
    fn sequenced_stream_requires_continuity() {
        let mut events = stream();
        for (index, event) in events.iter_mut().enumerate() {
            event["sequence_number"] = json!(index);
        }
        assert!(assemble(&events, 10).is_ok());
        events[3]["sequence_number"] = json!(2);
        assert!(assemble(&events, 10).is_err());
    }
}
