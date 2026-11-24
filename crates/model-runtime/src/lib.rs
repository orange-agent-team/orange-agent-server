//! Model adapters produce uncommitted candidates. Only the owning host may commit a response.
pub mod cache;
pub mod embeddings;
pub mod llama;
pub mod manifest;
pub mod meter_service;
pub mod metering;
mod responses;
pub mod stream;
#[cfg(unix)]
pub mod supervisor;
pub mod tool_schema;
#[cfg(unix)]
pub mod transcription;
pub use orange_protocol::model::{
    GenerateRequest, Message, NativeUsage, ResponseCandidate, Role, ToolCall, ToolDefinition,
};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid model configuration or request")]
    Invalid,
    #[error("model backend unavailable; no automatic inference retry")]
    Unavailable,
    #[error("model stream violated the selected protocol")]
    Protocol,
    #[error("model response exceeds configured bounds")]
    Limit,
    #[error("incomplete model response cannot authorize tools")]
    Incomplete,
    #[error("model request cancelled; backend resource reclamation is not implied")]
    Cancelled,
}

impl From<orange_protocol::model::ModelRequestError> for ModelError {
    fn from(error: orange_protocol::model::ModelRequestError) -> Self {
        match error {
            orange_protocol::model::ModelRequestError::Invalid => Self::Invalid,
            orange_protocol::model::ModelRequestError::Limit => Self::Limit,
        }
    }
}

impl From<ModelError> for orange_protocol::model::GatewayIssue {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::Invalid => Self::Invalid,
            ModelError::Unavailable => Self::Unavailable,
            ModelError::Protocol => Self::Protocol,
            ModelError::Limit => Self::Limit,
            ModelError::Incomplete => Self::Incomplete,
            ModelError::Cancelled => Self::Cancelled,
        }
    }
}
