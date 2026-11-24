//! Immutable model release metadata; references carry no command line or deployment credentials.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelVersion {
    pub schema_version: u32,
    pub model_version: String,
    pub source: ModelSource,
    pub artifacts: Vec<ModelArtifact>,
    pub runtime: RuntimePin,
    pub interface: ModelInterface,
    pub resources: ModelResources,
    pub pricing: PriceReference,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSource {
    pub repository: String,
    pub revision: String,
    pub license: String,
    pub asset_lock_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifact {
    pub source_path: String,
    pub cache_path: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum RuntimePin {
    Source {
        engine: String,
        source_revision: String,
        build_recipe_sha256: String,
    },
    Python {
        engine: String,
        dependency_lock_sha256: String,
        adapter_sha256: String,
    },
    Image {
        engine: String,
        image_digest: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "modality", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelInterface {
    Text {
        input: String,
        output: String,
        context_tokens: u32,
    },
    Embedding {
        input: String,
        output: String,
        max_batch: u32,
        max_tokens_per_text: u32,
        dimensions: u32,
    },
    Image {
        input: String,
        output: String,
        width: u32,
        height: u32,
        steps: u32,
        max_prompt_utf8_bytes: u32,
    },
    Transcription {
        input: String,
        output: String,
        channels: u16,
        sample_rate: u32,
        max_input_bytes: u64,
        max_decoded_ms: u32,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResources {
    pub device: String,
    pub artifact_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_vram_bytes: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum PriceReference {
    LocalUnpriced,
    PriceVersion { price_version: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifestReceipt {
    pub manifest_sha256: String,
    pub manifest: ModelVersion,
}

impl ModelVersion {
    pub fn manifest_sha256(&self) -> Result<String, serde_json::Error> {
        use sha2::{Digest, Sha256};
        Ok(hex::encode(Sha256::digest(serde_jcs::to_vec(self)?)))
    }
}
