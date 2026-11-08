//! Host-owned tool descriptions. Metadata is never an execution capability.
use crate::{model::ToolDefinition, request_digest, valid_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SPEC_VERSION: u16 = 1;
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// Count serialized bytes without allocating an unbounded second copy.
pub fn encoded_within_limit(value: &impl Serialize, limit: usize) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("encoded tool payload limit"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(limit), value).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    ReadOnly,
    LocalWrite,
    Process,
    RemoteWrite,
    ModelInvocation,
    UserInput,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolLimits {
    /// Execution after resource admission; queue/wait duration is governed by Run deadlines.
    /// None is only valid for user interaction, bounded by the containing Run deadline.
    pub execution_timeout_ms: Option<u32>,
    pub max_argument_bytes: u32,
    pub max_result_bytes: u32,
    pub max_model_result_bytes: u32,
}
fn unknown() -> String {
    "unknown".into()
}
fn exclusive() -> String {
    "exclusive".into()
}
fn required() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub spec_version: u16,
    pub name: String,
    pub tool_version: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    /// Preserve unknown declarations in the digest; never reinterpret them as readonly.
    #[serde(default = "unknown")]
    pub effect_class: String,
    #[serde(default = "exclusive")]
    pub concurrency: String,
    #[serde(default = "required")]
    pub approval_required: bool,
    pub execution_environment: String,
    pub limits: ToolLimits,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecError {
    Invalid,
    Limit,
    Schema,
}
impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool specification rejected: {self:?}")
    }
}
impl std::error::Error for SpecError {}
impl ToolSpec {
    pub fn effect(&self) -> EffectClass {
        match self.effect_class.as_str() {
            "read_only" => EffectClass::ReadOnly,
            "local_write" => EffectClass::LocalWrite,
            "process" => EffectClass::Process,
            "remote_write" => EffectClass::RemoteWrite,
            "model_invocation" => EffectClass::ModelInvocation,
            "user_input" => EffectClass::UserInput,
            _ => EffectClass::Unknown,
        }
    }
    pub fn parallel_safe(&self) -> bool {
        self.effect() == EffectClass::ReadOnly && self.concurrency == "parallel_read"
    }
    pub fn needs_approval(&self) -> bool {
        self.approval_required
            || !matches!(
                self.effect(),
                EffectClass::ReadOnly | EffectClass::UserInput
            )
    }
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.spec_version != SPEC_VERSION
            || !valid_id(&self.name)
            || self.name.len() > 64
            || !valid_id(&self.tool_version)
            || self.tool_version.len() > 128
            || self.description.trim().is_empty()
            || self.description.len() > 8192
            || ![
                &self.effect_class,
                &self.concurrency,
                &self.execution_environment,
            ]
            .iter()
            .all(|s| valid_id(s) && s.len() <= 64)
        {
            return Err(SpecError::Invalid);
        }
        let l = &self.limits;
        if l.execution_timeout_ms
            .is_some_and(|v| !(1..=300_000).contains(&v))
            || (l.execution_timeout_ms.is_none()
                && !(self.effect() == EffectClass::UserInput
                    && self.execution_environment == "host_interaction"))
            || !(1..=1024 * 1024).contains(&l.max_argument_bytes)
            || !(1..=2 * 1024 * 1024).contains(&l.max_result_bytes)
            || l.max_model_result_bytes == 0
            || l.max_model_result_bytes > l.max_result_bytes
        {
            return Err(SpecError::Limit);
        }
        for schema in [&self.input_schema, &self.output_schema] {
            if !bounded_schema(schema) || !encoded_within_limit(schema, MAX_SCHEMA_BYTES) {
                return Err(SpecError::Schema);
            }
        }
        Ok(())
    }
    /// Description wording is deliberately excluded. Execution-relevant fields
    /// and both schemas are pinned; a changed effect/limit/version changes the pin.
    pub fn execution_digest(&self) -> Result<String, SpecError> {
        self.validate()?;
        request_digest("tool-spec-execution.v1",&serde_json::json!({
            "spec_version":self.spec_version,"name":self.name,"tool_version":self.tool_version,
            "input_schema":self.input_schema,"output_schema":self.output_schema,
            "effect_class":self.effect_class,"concurrency":self.concurrency,
            "approval_required":self.approval_required,"execution_environment":self.execution_environment,
            "limits":self.limits
        })).map_err(|_|SpecError::Invalid)
    }
    pub fn model_definition(&self) -> Result<ToolDefinition, SpecError> {
        self.validate()?;
        Ok(ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
        })
    }
}
fn nodes(value: &Value, depth: usize, budget: &mut usize) -> bool {
    if depth > 32 || *budget == 0 {
        return false;
    }
    *budget -= 1;
    match value {
        Value::Object(v) => v.values().all(|v| nodes(v, depth + 1, budget)),
        Value::Array(v) => v.iter().all(|v| nodes(v, depth + 1, budget)),
        _ => true,
    }
}
fn references(root: &Value, value: &Value, depth: usize, budget: &mut usize) -> bool {
    if depth > 32 || *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Value::Object(map) = value else {
        return value.is_boolean();
    };
    if map.contains_key("$dynamicRef")
        || map.contains_key("$recursiveRef")
        || map
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
        if !references(root, target, depth + 1, budget) {
            return false;
        }
    }
    map.iter().all(|(key, v)| match key.as_str() {
        "properties" | "patternProperties" | "$defs" | "dependentSchemas" => v
            .as_object()
            .is_some_and(|m| m.values().all(|s| references(root, s, depth + 1, budget))),
        "allOf" | "anyOf" | "oneOf" | "prefixItems" => v
            .as_array()
            .is_some_and(|a| a.iter().all(|s| references(root, s, depth + 1, budget))),
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
        | "contentSchema" => references(root, v, depth + 1, budget),
        _ => true,
    })
}
pub fn bounded_schema(value: &Value) -> bool {
    nodes(value, 0, &mut 4096) && references(value, value, 0, &mut 4096)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    pub fn fixture() -> ToolSpec {
        ToolSpec {
            spec_version: 1,
            name: "fixture".into(),
            tool_version: "fixture.v1".into(),
            description: "test descriptor, no executor".into(),
            input_schema: json!({"type":"object","additionalProperties":false}),
            output_schema: json!({"type":"object","additionalProperties":false}),
            effect_class: "read_only".into(),
            concurrency: "parallel_read".into(),
            approval_required: false,
            execution_environment: "workspace".into(),
            limits: ToolLimits {
                execution_timeout_ms: Some(10000),
                max_argument_bytes: 65536,
                max_result_bytes: 65536,
                max_model_result_bytes: 65536,
            },
        }
    }
    #[test]
    fn encoded_byte_limits_count_json_escaping_without_a_second_large_buffer() {
        let value = json!({"text":"\u{1}中文"});
        let bytes = serde_json::to_vec(&value).expect("encoding").len();
        assert!(encoded_within_limit(&value, bytes));
        assert!(!encoded_within_limit(&value, bytes - 1));
        assert!(!encoded_within_limit(
            &json!({"text":"x".repeat(100_000)}),
            64
        ));
    }
    #[test]
    fn unknown_effects_remain_distinct_conservative_metadata() {
        let mut original = serde_json::to_value(fixture()).expect("serialize");
        original
            .as_object_mut()
            .expect("object")
            .remove("effect_class");
        let missing: ToolSpec = serde_json::from_value(original.clone()).expect("default");
        assert_eq!(missing.effect(), EffectClass::Unknown);
        assert!(!missing.parallel_safe());
        assert!(missing.needs_approval());
        original["effect_class"] = json!("future_external_write");
        let a: ToolSpec = serde_json::from_value(original.clone()).expect("unknown");
        original["effect_class"] = json!("future_other_effect");
        let b: ToolSpec = serde_json::from_value(original.clone()).expect("unknown");
        assert_ne!(a.execution_digest(), b.execution_digest());
        assert!(a.needs_approval() && !a.parallel_safe());
        original["grant_access"] = json!(true);
        assert!(serde_json::from_value::<ToolSpec>(original).is_err());
    }
    #[test]
    fn execution_pin_covers_schema_policy_and_limits_but_not_copy_edits() {
        let spec = fixture();
        let pin = spec.execution_digest().expect("digest");
        let mut copy = spec.clone();
        copy.description = "clearer help text".into();
        assert_eq!(copy.execution_digest().expect("digest"), pin);
        for changed in [
            "schema",
            "output",
            "effect",
            "version",
            "limits",
            "environment",
            "approval",
            "concurrency",
        ] {
            let mut copy = spec.clone();
            match changed {
                "schema" => copy.input_schema = json!({"type":"string"}),
                "output" => copy.output_schema = json!({"type":"string"}),
                "effect" => copy.effect_class = "local_write".into(),
                "version" => copy.tool_version = "fixture.v2".into(),
                "limits" => copy.limits.max_result_bytes += 1,
                "environment" => copy.execution_environment = "remote".into(),
                "approval" => copy.approval_required = true,
                _ => copy.concurrency = "exclusive".into(),
            };
            assert_ne!(copy.execution_digest().expect("digest"), pin, "{changed}");
        }
    }
    #[test]
    fn schemas_are_bounded_offline_and_cannot_recurse_or_fetch() {
        for bad in [
            json!({"$ref":"https://example.invalid/schema"}),
            json!({"$ref":"file:///private/key"}),
            json!({"$ref":"#"}),
            json!({"$defs":{"self":{"$ref":"#/$defs/self"}},"$ref":"#/$defs/self"}),
            json!({"$dynamicRef":"#x"}),
            json!({"enum":vec![0;4096]}),
        ] {
            let mut spec = fixture();
            spec.output_schema = bad;
            assert_eq!(spec.validate(), Err(SpecError::Schema));
        }
        let mut spec = fixture();
        spec.input_schema = json!({"$defs":{"path":{"type":"string"}},"type":"object","properties":{"path":{"$ref":"#/$defs/path"}}});
        assert!(spec.validate().is_ok());
        spec.limits.execution_timeout_ms = Some(0);
        assert_eq!(spec.validate(), Err(SpecError::Limit));
    }
}
