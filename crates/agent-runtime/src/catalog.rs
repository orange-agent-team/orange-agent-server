//! Immutable, offline-validated tool descriptors. This registry contains no
//! callable handlers and cannot grant workspace, process or connector access.
mod builtin;
use orange_protocol::{
    model::ToolDefinition,
    request_digest,
    tool_spec::{SpecError, ToolSpec, encoded_within_limit},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error("tool catalog limit exceeded")]
    Limit,
    #[error("tool name is ambiguous")]
    Duplicate,
    #[error("tool is not registered in this snapshot")]
    Unavailable,
    #[error("tool schema or version changed")]
    PinChanged,
    #[error("tool catalog document changed")]
    SnapshotChanged,
    #[error("invalid or unsupported offline tool schema")]
    Schema,
    #[error("tool payload does not match its pinned schema")]
    Payload,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolPin {
    pub name: String,
    pub tool_version: String,
    pub execution_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDocument {
    /// Execution revision excludes wording/order; document digest covers both.
    pub revision: String,
    pub document_digest: String,
    pub tools: Vec<ToolSpec>,
}
struct Entry {
    spec: ToolSpec,
    pin: ToolPin,
    input: jsonschema::Validator,
    output: jsonschema::Validator,
}
pub struct ToolCatalog {
    revision: String,
    document_digest: String,
    order: Vec<String>,
    entries: BTreeMap<String, Entry>,
}
#[derive(Clone, Copy)]
pub enum OutputChannel {
    Durable,
    Model,
}
fn compile(schema: &Value) -> Result<jsonschema::Validator, CatalogError> {
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
        .map_err(|_| CatalogError::Schema)
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
fn payload(value: &Value, limit: u32) -> Result<(), CatalogError> {
    if !nodes(value, 0, &mut 4096) || !encoded_within_limit(value, limit as usize) {
        return Err(CatalogError::Limit);
    }
    Ok(())
}

impl ToolCatalog {
    pub fn new(specs: Vec<ToolSpec>) -> Result<Self, CatalogError> {
        if specs.len() > 80 {
            return Err(CatalogError::Limit);
        }
        for spec in &specs {
            spec.validate()?;
        }
        if !encoded_within_limit(&specs, 2 * 1024 * 1024) {
            return Err(CatalogError::Limit);
        }
        let document_digest =
            request_digest("tool-catalog-document.v1", &specs).map_err(|_| CatalogError::Schema)?;
        let order = specs.iter().map(|s| s.name.clone()).collect();
        let mut entries = BTreeMap::new();
        for spec in specs {
            spec.validate()?;
            if entries.contains_key(&spec.name) {
                return Err(CatalogError::Duplicate);
            }
            let pin = ToolPin {
                name: spec.name.clone(),
                tool_version: spec.tool_version.clone(),
                execution_digest: spec.execution_digest()?,
            };
            let input = compile(&spec.input_schema)?;
            let output = compile(&spec.output_schema)?;
            entries.insert(
                spec.name.clone(),
                Entry {
                    spec,
                    pin,
                    input,
                    output,
                },
            );
        }
        let pins: Vec<_> = entries.values().map(|e| &e.pin).collect();
        let revision =
            request_digest("tool-catalog.v1", &pins).map_err(|_| CatalogError::Schema)?;
        Ok(Self {
            revision,
            document_digest,
            order,
            entries,
        })
    }
    pub fn builtin(questions: bool, creation: bool) -> Result<Self, CatalogError> {
        Self::new(builtin::specs(questions, creation))
    }
    pub fn restore(document: CatalogDocument) -> Result<Self, CatalogError> {
        let catalog = Self::new(document.tools)?;
        if catalog.revision != document.revision {
            return Err(CatalogError::PinChanged);
        }
        if catalog.document_digest != document.document_digest {
            return Err(CatalogError::SnapshotChanged);
        }
        Ok(catalog)
    }
    pub fn document(&self) -> CatalogDocument {
        CatalogDocument {
            revision: self.revision.clone(),
            document_digest: self.document_digest.clone(),
            tools: self
                .order
                .iter()
                .map(|name| self.entries[name].spec.clone())
                .collect(),
        }
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn describe(&self, name: &str) -> Option<&ToolSpec> {
        self.entries.get(name).map(|e| &e.spec)
    }
    pub fn pin(&self, name: &str) -> Result<ToolPin, CatalogError> {
        self.entries
            .get(name)
            .map(|e| e.pin.clone())
            .ok_or(CatalogError::Unavailable)
    }
    pub fn definitions(&self) -> Result<Vec<ToolDefinition>, CatalogError> {
        self.order
            .iter()
            .map(|name| {
                self.entries[name]
                    .spec
                    .model_definition()
                    .map_err(Into::into)
            })
            .collect()
    }
    fn entry(&self, pin: &ToolPin) -> Result<&Entry, CatalogError> {
        let entry = self
            .entries
            .get(&pin.name)
            .ok_or(CatalogError::Unavailable)?;
        if &entry.pin != pin {
            return Err(CatalogError::PinChanged);
        }
        Ok(entry)
    }
    pub fn validate_input(&self, pin: &ToolPin, value: &Value) -> Result<(), CatalogError> {
        let e = self.entry(pin)?;
        payload(value, e.spec.limits.max_argument_bytes)?;
        if !e.input.is_valid(value) {
            return Err(CatalogError::Payload);
        }
        Ok(())
    }
    pub fn validate_output(
        &self,
        pin: &ToolPin,
        value: &Value,
        channel: OutputChannel,
    ) -> Result<(), CatalogError> {
        let e = self.entry(pin)?;
        payload(
            value,
            match channel {
                OutputChannel::Durable => e.spec.limits.max_result_bytes,
                OutputChannel::Model => e.spec.limits.max_model_result_bytes,
            },
        )?;
        if !e.output.is_valid(value) {
            return Err(CatalogError::Payload);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests;
