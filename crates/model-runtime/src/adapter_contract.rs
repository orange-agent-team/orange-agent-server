//! HTTP operations permitted by each locally implemented text/vector adapter.
use crate::{ModelError, embeddings, manifest};
use orange_protocol::model_versions::{ModelInterface, ModelVersion, RuntimePin};
use serde::Serialize;

pub(crate) const EMBEDDINGS_PATH: &str = "v1/embeddings";
pub(crate) const CHAT_PATH: &str = "v1/chat/completions";
pub(crate) const RESPONSES_PATH: &str = "v1/responses";

#[derive(Debug, Serialize)]
#[serde(tag = "modality", rename_all = "snake_case")]
pub enum LocalAdapterSpec {
    Text {
        context_tokens: u32,
        generation_paths: [&'static str; 2],
        chat_template: &'static str,
        tokenizer_path: &'static str,
    },
    Embedding {
        path: &'static str,
        max_batch: u32,
        max_tokens_per_text: u32,
        dimensions: u32,
        encoding: &'static str,
    },
}

impl LocalAdapterSpec {
    /// Use only an installed, version-locked release. An arbitrary draft is not a transport configuration.
    pub fn bundled(model: &str) -> Result<Self, ModelError> {
        let version = manifest::find(model).map_err(|_| ModelError::Invalid)?;
        Self::for_version(&version.receipt().manifest)
    }

    pub(crate) fn for_version(manifest: &ModelVersion) -> Result<Self, ModelError> {
        match (&manifest.runtime, &manifest.interface) {
            (RuntimePin::Source { engine, .. }, ModelInterface::Text { context_tokens, .. })
                if engine == "llama_cpp" =>
            {
                Ok(Self::Text {
                    context_tokens: *context_tokens,
                    generation_paths: [CHAT_PATH, RESPONSES_PATH],
                    chat_template: "gguf_embedded",
                    tokenizer_path: "tokenize",
                })
            }
            (
                RuntimePin::Python { engine, .. },
                ModelInterface::Embedding {
                    max_batch,
                    max_tokens_per_text,
                    dimensions,
                    ..
                },
            ) if engine == "onnx_runtime"
                && *max_batch == embeddings::MAX_BATCH as u32
                && *dimensions == embeddings::DIMENSIONS as u32
                && *max_tokens_per_text == embeddings::MAX_TOKENS_PER_TEXT =>
            {
                Ok(Self::Embedding {
                    path: EMBEDDINGS_PATH,
                    max_batch: *max_batch,
                    max_tokens_per_text: *max_tokens_per_text,
                    dimensions: *dimensions,
                    encoding: "float",
                })
            }
            _ => Err(ModelError::Invalid),
        }
    }
}
