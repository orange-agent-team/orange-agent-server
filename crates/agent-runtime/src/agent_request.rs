//! Preserve historical workspace JSON/hashes while separating non-filesystem
//! requests at the type boundary. A WorkspaceGrant cannot execute MemoryRequest.
use crate::{
    memory::tools::MemoryRequest,
    workspace::{WorkspaceError, WorkspaceRequest},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentRequest {
    Workspace(WorkspaceRequest),
    Memory(MemoryRequest),
}
impl AgentRequest {
    pub fn parse(name: &str, args: &Value) -> Result<Self, WorkspaceError> {
        if matches!(name, "memory_read" | "memory_write") {
            Ok(Self::Memory(
                MemoryRequest::parse(name, args).map_err(|_| WorkspaceError::Invalid)?,
            ))
        } else {
            Ok(Self::Workspace(WorkspaceRequest::parse(name, args)?))
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Workspace(v) => v.name(),
            Self::Memory(v) => v.name(),
        }
    }
    pub fn version(&self) -> &'static str {
        match self {
            Self::Workspace(v) => v.version(),
            Self::Memory(v) => v.version(),
        }
    }
    pub fn digest(&self) -> Result<String, WorkspaceError> {
        match self {
            Self::Workspace(v) => v.digest(),
            Self::Memory(v) => v.digest().map_err(|_| WorkspaceError::Invalid),
        }
    }
    pub fn workspace(&self) -> Result<&WorkspaceRequest, WorkspaceError> {
        match self {
            Self::Workspace(v) => Ok(v),
            Self::Memory(_) => Err(WorkspaceError::Forbidden),
        }
    }
}
impl From<WorkspaceRequest> for AgentRequest {
    fn from(v: WorkspaceRequest) -> Self {
        Self::Workspace(v)
    }
}
