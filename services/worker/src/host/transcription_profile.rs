use super::config::private_file;
use orange_model_runtime::transcription::{Client, Qualification};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeConfig {
    python: PathBuf,
    model_cache: PathBuf,
    private_slot: PathBuf,
    expected_execution_profile_digest: Option<String>,
}

pub async fn probe(
    path: &Path,
    cancel: watch::Receiver<bool>,
) -> Result<Qualification, Box<dyn std::error::Error>> {
    let config: ProbeConfig = serde_json::from_slice(&private_file(path, 16 * 1024)?)?;
    let client = Client::new(&config.python, &config.model_cache, &config.private_slot)?;
    Ok(client
        .qualify(config.expected_execution_profile_digest.as_deref(), cancel)
        .await?)
}
