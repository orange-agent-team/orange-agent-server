use super::*;
use orange_protocol::tool_spec::ToolLimits;
use serde_json::json;
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn text() -> Value {
    json!({"type":"string"})
}
fn decimal() -> Value {
    json!({"type":"string","pattern":"^(0|[1-9][0-9]*)$","maxLength":20})
}
fn hash() -> Value {
    json!({"type":"string","pattern":"^[a-f0-9]{64}$"})
}
fn nullable(value: Value) -> Value {
    json!({"anyOf":[value,{"type":"null"}]})
}
fn tagged(name: &str, data: Value) -> Value {
    object(
        json!({"kind":{"const":name},"data":data}),
        &["kind", "data"],
    )
}
fn output(name: &str) -> Value {
    let data = match name {
        "read_file" => object(
            json!({"path":text(),"sha256":hash(),"total_bytes":decimal(),"start_byte":decimal(),"end_byte":decimal(),"content":text(),"truncated":{"type":"boolean"},"next_byte":nullable(decimal())}),
            &[
                "path",
                "sha256",
                "total_bytes",
                "start_byte",
                "end_byte",
                "content",
                "truncated",
                "next_byte",
            ],
        ),
        "list_directory" => {
            let entry = object(
                json!({"name":text(),"kind":{"enum":["file","directory"]},"bytes":nullable(decimal())}),
                &["name", "kind", "bytes"],
            );
            object(
                json!({"path":text(),"snapshot_digest":hash(),"entries":{"type":"array","maxItems":200,"items":entry},"next_cursor":nullable(text()),"omitted_unsupported":{"type":"boolean"}}),
                &[
                    "path",
                    "snapshot_digest",
                    "entries",
                    "next_cursor",
                    "omitted_unsupported",
                ],
            )
        }
        "search_files" => object(
            json!({"path":text(),"paths":{"type":"array","maxItems":200,"items":text()},"scanned_entries":{"type":"integer","minimum":0,"maximum":10000},"truncated":{"type":"boolean"},"omitted_unsupported":{"type":"boolean"}}),
            &[
                "path",
                "paths",
                "scanned_entries",
                "truncated",
                "omitted_unsupported",
            ],
        ),
        "search_text" => {
            let entry = object(
                json!({"path":text(),"sha256":hash(),"line":decimal(),"start_byte":decimal(),"end_byte":decimal(),"excerpt":text(),"excerpt_truncated":{"type":"boolean"}}),
                &[
                    "path",
                    "sha256",
                    "line",
                    "start_byte",
                    "end_byte",
                    "excerpt",
                    "excerpt_truncated",
                ],
            );
            object(
                json!({"path":text(),"matches":{"type":"array","maxItems":200,"items":entry},"scanned_files":{"type":"integer","minimum":0,"maximum":10000},"scanned_bytes":decimal(),"truncated":{"type":"boolean"},"omitted_unsupported":{"type":"boolean"}}),
                &[
                    "path",
                    "matches",
                    "scanned_files",
                    "scanned_bytes",
                    "truncated",
                    "omitted_unsupported",
                ],
            )
        }
        "write_file" => object(
            json!({"path":text(),"sha256":hash(),"bytes":decimal()}),
            &["path", "sha256", "bytes"],
        ),
        "ask_user" => {
            return object(
                json!({"answers":{"type":"array","minItems":1,"maxItems":3,"items":{"type":"string","minLength":1,"maxLength":4096}}}),
                &["answers"],
            );
        }
        _ => return Value::Bool(false),
    };
    json!({"oneOf":[tagged(name,data),object(json!({"error":{"type":"string","pattern":"^[a-z_]{1,64}$"}}),&["error"])]})
}
pub(super) fn specs(questions: bool, creation: bool) -> Vec<ToolSpec> {
    let mut definitions = crate::workspace::definitions();
    if questions {
        definitions.push(orange_protocol::questions::definition());
    }
    if creation {
        definitions.push(crate::workspace::creation::definition());
    }
    definitions
        .into_iter()
        .map(|d| {
            let question = d.name == "ask_user";
            let write = d.name == "write_file";
            ToolSpec {
                spec_version: 1,
                tool_version: format!("{}.v1", d.name),
                output_schema: output(&d.name),
                name: d.name,
                description: d.description,
                input_schema: d.parameters,
                effect_class: if question {
                    "user_input"
                } else if write {
                    "local_write"
                } else {
                    "read_only"
                }
                .into(),
                concurrency: if question || write {
                    "exclusive"
                } else {
                    "parallel_read"
                }
                .into(),
                approval_required: !question,
                execution_environment: if question {
                    "host_interaction"
                } else {
                    "workspace"
                }
                .into(),
                limits: ToolLimits {
                    execution_timeout_ms: if question { None } else { Some(10000) },
                    max_argument_bytes: 256 * 1024,
                    max_result_bytes: if question {
                        128 * 1024
                    } else {
                        2 * 1024 * 1024
                    },
                    max_model_result_bytes: if question { 128 * 1024 } else { 64 * 1024 },
                },
            }
        })
        .collect()
}
