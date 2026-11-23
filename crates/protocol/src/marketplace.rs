//! Portable typed references for reviewing a marketplace workload. Authority is resolved by the serving system.
use orange_domain::identity::{ArtifactPin, InvalidId, Milliseconds};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub version_id: String,
    pub sha256: String,
}
impl ArtifactRef {
    pub fn pin(&self) -> Result<ArtifactPin<'_>, InvalidId> {
        ArtifactPin::new(&self.artifact_id, &self.version_id, &self.sha256)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobInputRef {
    Text {
        source: ArtifactRef,
    },
    Embedding {
        source: ArtifactRef,
    },
    Image {
        source: ArtifactRef,
    },
    Transcription {
        source: ArtifactRef,
        duration_ms: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub enum InputReferenceError {
    InvalidPin,
    InvalidDuration,
}
impl JobInputRef {
    pub fn pin(&self) -> Result<ArtifactPin<'_>, InputReferenceError> {
        match self {
            Self::Text { source } | Self::Embedding { source } | Self::Image { source } => {
                source.pin().map_err(|_| InputReferenceError::InvalidPin)
            }
            Self::Transcription {
                source,
                duration_ms,
            } => {
                if duration_ms
                    .parse::<Milliseconds>()
                    .map_err(|_| InputReferenceError::InvalidDuration)?
                    .get()
                    == 0
                {
                    return Err(InputReferenceError::InvalidDuration);
                }
                source.pin().map_err(|_| InputReferenceError::InvalidPin)
            }
        }
    }
}
