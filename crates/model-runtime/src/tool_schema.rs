//! Versioned offline tool-schema admission; no filesystem/network reference IO.
use crate::{GenerateRequest, ResponseCandidate};
use serde_json::Value;
pub const RULE: &str = "jsonschema-0.56-draft202012-offline-bounded-v1";
fn nodes(value: &Value, depth: usize, budget: &mut usize) -> bool {
    if depth > 32 || *budget == 0 {
        return false;
    }
    *budget -= 1;
    match value {
        Value::Object(m) => m.values().all(|v| nodes(v, depth + 1, budget)),
        Value::Array(a) => a.iter().all(|v| nodes(v, depth + 1, budget)),
        _ => true,
    }
}
fn bounded(root: &Value, value: &Value, depth: usize, budget: &mut usize) -> bool {
    if depth > 32 || *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Value::Object(map) = value else {
        return value.is_boolean();
    };
    if map.contains_key("$dynamicRef") || map.contains_key("$recursiveRef") {
        return false;
    }
    if map
        .get("$schema")
        .is_some_and(|v| v.as_str() != Some("https://json-schema.org/draft/2020-12/schema"))
    {
        return false;
    }
    if let Some(reference) = map.get("$ref") {
        let Some(pointer) = reference
            .as_str()
            .and_then(|r| r.strip_prefix('#'))
            .filter(|r| r.starts_with('/'))
        else {
            return false;
        };
        let Some(target) = root.pointer(pointer) else {
            return false;
        };
        if !bounded(root, target, depth + 1, budget) {
            return false;
        }
    }
    for (key, v) in map {
        let valid = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "dependentSchemas" => v
                .as_object()
                .is_some_and(|m| m.values().all(|s| bounded(root, s, depth + 1, budget))),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => v
                .as_array()
                .is_some_and(|a| a.iter().all(|s| bounded(root, s, depth + 1, budget))),
            "items"
            | "contains"
            | "additionalProperties"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => bounded(root, v, depth + 1, budget),
            _ => true,
        };
        if !valid {
            return false;
        }
    }
    true
}
fn compile(schema: &Value) -> Result<jsonschema::Validator, ()> {
    if serde_json::to_vec(schema).map_err(|_| ())?.len() > 64 * 1024
        || !bounded(schema, schema, 0, &mut 4096)
    {
        return Err(());
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .offline()
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .with_pattern_options(
            jsonschema::PatternOptions::regex()
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024),
        )
        .build(schema)
        .map_err(|_| ())
}
pub fn validate_request(request: &GenerateRequest) -> Result<(), crate::ModelError> {
    request.validate()?;
    for tool in &request.tools {
        compile(&tool.parameters).map_err(|_| crate::ModelError::Invalid)?;
    }
    Ok(())
}
pub fn validate_response(
    request: &GenerateRequest,
    candidate: &ResponseCandidate,
) -> Result<(), crate::ModelError> {
    validate_request(request)?;
    crate::metering::project_candidate(candidate).map_err(|_| crate::ModelError::Invalid)?;
    if !orange_protocol::valid_id(&candidate.response_id) {
        return Err(crate::ModelError::Invalid);
    }
    for call in &candidate.tool_calls {
        if !call.arguments.is_object()
            || request
                .messages
                .iter()
                .any(|m| m.tool_calls.iter().any(|p| p.id == call.id))
        {
            return Err(crate::ModelError::Invalid);
        }
        let tool = request
            .tools
            .iter()
            .find(|t| t.name == call.name)
            .ok_or(crate::ModelError::Invalid)?;
        let mut argument_budget = 4096;
        let mut schema_budget = 4096;
        if serde_json::to_vec(&call.arguments)
            .map_err(|_| crate::ModelError::Invalid)?
            .len()
            > 64 * 1024
            || !nodes(&call.arguments, 0, &mut argument_budget)
            || !bounded(&tool.parameters, &tool.parameters, 0, &mut schema_budget)
            || (4096 - argument_budget) * (4096 - schema_budget) > 65536
        {
            return Err(crate::ModelError::Limit);
        }
        if !compile(&tool.parameters)
            .map_err(|_| crate::ModelError::Invalid)?
            .is_valid(&call.arguments)
        {
            return Err(crate::ModelError::Invalid);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_bounded_schema_enforces_arguments_and_rejects_external_or_recursive_refs() {
        let s = serde_json::json!({"type":"object","properties":{"path":{"$ref":"#/$defs/path"}},"required":["path"],"additionalProperties":false,"$defs":{"path":{"type":"string","minLength":1}}});
        let v = compile(&s).expect("local acyclic reference");
        assert!(v.is_valid(&serde_json::json!({"path":"a"})));
        assert!(!v.is_valid(&serde_json::json!({"path":3})));
        assert!(!v.is_valid(&serde_json::json!({"path":"a","extra":true})));
        for r in [
            "https://example.invalid/schema",
            "file:///private/test",
            "#",
        ] {
            assert!(compile(&serde_json::json!({"$ref":r})).is_err());
        }
        assert!(
            compile(
                &serde_json::json!({"$defs":{"loop":{"$ref":"#/$defs/loop"}},"$ref":"#/$defs/loop"})
            )
            .is_err()
        );
        assert!(
            compile(&serde_json::json!({"type":"string","format":"unregistered-format"})).is_err()
        );
        let data_key = serde_json::json!({"type":"object","properties":{"$ref":{"type":"string"}},"required":["$ref"]});
        assert!(
            compile(&data_key)
                .expect("keyword-looking property is ordinary data")
                .is_valid(&serde_json::json!({"$ref":"not a schema reference"}))
        );
    }
}
