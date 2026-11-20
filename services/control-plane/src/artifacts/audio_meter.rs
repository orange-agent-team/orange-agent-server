//! Authorize an immutable uploaded object, measure it locally, then reauthorize delivery.
use super::*;
use orange_model_runtime::transcription::{Client, Error as ModelError};
use orange_protocol::pricing::AudioMeasurement;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone)]
pub struct AudioMeter {
    client: Client,
    admission: Arc<Semaphore>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    python: PathBuf,
    model_cache: PathBuf,
    private_slot: PathBuf,
}
impl AudioMeter {
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let cfg: Config =
            serde_json::from_slice(&crate::worker::bootstrap::private_configuration(path)?)?;
        Ok(Self::new(Client::new(
            &cfg.python,
            &cfg.model_cache,
            &cfg.private_slot,
        )?))
    }
    fn new(client: Client) -> Self {
        Self {
            client,
            admission: Arc::new(Semaphore::new(1)),
        }
    }

    pub(super) async fn measure(
        &self,
        store: &ArtifactStore,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
    ) -> Result<AudioMeasurement, ArtifactError> {
        if !valid_id(upload) || !valid_id(version) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = store.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        let row = store.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        let descriptor = view(&row)?;
        if descriptor.version_id != version {
            return Err(ArtifactError::Conflict);
        }
        let format = match descriptor.media_type.as_str() {
            "audio/wav" => "wav",
            "audio/flac" => "flac",
            "audio/ogg" => "ogg",
            "audio/mp4" => "mov",
            "audio/mpeg" => "mp3",
            _ => return Err(ArtifactError::Invalid),
        };
        authority::finish(tx, actor).await?;
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| ArtifactError::Busy)?;
        let (source, data, stamp) = store.read(actor, owner, upload, version).await?;
        let (_stop, cancel) = tokio::sync::watch::channel(false);
        let result =
            self.client
                .measure(&data.to_vec(), cancel)
                .await
                .map_err(|error| match error {
                    ModelError::Invalid | ModelError::Limit | ModelError::NoSpeech => {
                        ArtifactError::Invalid
                    }
                    ModelError::Busy => ArtifactError::Busy,
                    ModelError::Timeout => ArtifactError::Expired,
                    ModelError::Changed | ModelError::Protocol => ArtifactError::Integrity,
                    ModelError::Cancelled | ModelError::Unavailable => ArtifactError::Unavailable,
                });
        // Revocation also suppresses codec/error information learned by the decoder.
        store.check_read_scope(actor, &source, &stamp, true).await?;
        let result = result?;
        let measured = result.measure;
        if result.audio_sha256 != source.sha256 {
            return Err(ArtifactError::Integrity);
        }
        if measured.format != format {
            return Err(ArtifactError::Invalid);
        }
        Ok(AudioMeasurement {
            artifact_id: source.artifact_id,
            version_id: source.version_id,
            audio_sha256: result.audio_sha256,
            model_version: result.model_version,
            preprocessing: measured.preprocessing,
            codec: measured.codec,
            format: measured.format,
            source_sample_rate: measured.sample_rate,
            source_samples: measured.source_samples.to_string(),
            normalized_samples: measured.normalized_samples.to_string(),
            normalized_sha256: measured.normalized_sha256,
            duration_ms: measured.duration_ms.to_string(),
        })
    }
}
