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

pub(super) async fn prepare(
    path: &Path,
    owned_slot: Option<&Path>,
    cancel: watch::Receiver<bool>,
) -> Result<(Client, Qualification), Box<dyn std::error::Error>> {
    let config: ProbeConfig = serde_json::from_slice(&private_file(path, 16 * 1024)?)?;
    let client = Client::new(
        &config.python,
        &config.model_cache,
        owned_slot.unwrap_or(&config.private_slot),
    )?;
    let fact = client
        .qualify(config.expected_execution_profile_digest.as_deref(), cancel)
        .await?;
    Ok((client, fact))
}
pub async fn probe(
    path: &Path,
    cancel: watch::Receiver<bool>,
) -> Result<Qualification, Box<dyn std::error::Error>> {
    Ok(prepare(path, None, cancel).await?.1)
}
