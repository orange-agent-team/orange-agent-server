//! Verification of independently signed development model certification evidence. No signing secret enters the service.
use super::AdminError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use orange_protocol::model_versions::{ModelInterface, ModelVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct ModelReviewTrust {
    pub(super) key: [u8; 32],
    pub(super) fingerprint: String,
}
impl ModelReviewTrust {
    pub fn load(path: &std::path::Path) -> Result<Self, AdminError> {
        use std::io::Read;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        if !path.is_absolute()
            || !std::fs::symlink_metadata(path)
                .map_err(|_| AdminError::Invalid)?
                .is_file()
        {
            return Err(AdminError::Invalid);
        }
        let f = opts.open(path).map_err(|_| AdminError::Invalid)?;
        let meta = f.metadata().map_err(|_| AdminError::Invalid)?;
        if !meta.is_file() || meta.len() > 66 {
            return Err(AdminError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.mode() & 0o022 != 0 {
                return Err(AdminError::Invalid);
            }
        }
        let mut raw = String::new();
        f.take(67)
            .read_to_string(&mut raw)
            .map_err(|_| AdminError::Invalid)?;
        let line = raw.strip_suffix('\n').unwrap_or(&raw);
        let text = line.strip_suffix('\r').unwrap_or(line);
        if !orange_protocol::valid_digest(text) || text == "0".repeat(64) {
            return Err(AdminError::Invalid);
        }
        let key: [u8; 32] = hex::decode(text)
            .map_err(|_| AdminError::Invalid)?
            .try_into()
            .map_err(|_| AdminError::Invalid)?;
        Ok(Self {
            fingerprint: hex::encode(Sha256::digest(key)),
            key,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    pub report: Report,
    pub signing_key_sha256: String,
    signature: String,
    license_notice: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Report {
    version: u32,
    purpose: String,
    environment: String,
    pub model_version_id: String,
    pub manifest_sha256: String,
    pub model_sha256: String,
    pub execution_profile_id: String,
    pub execution_profile_digest: String,
    pub issued_unix: String,
    pub expires_unix: String,
    license: License,
    measurement: Measurement,
    hardware: Hardware,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct License {
    identifier: String,
    source_revision: String,
    notice_sha256: String,
    permitted_use: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Measurement {
    runtime_sha256: String,
    verified_asset_count: u32,
    verified_asset_bytes: u64,
    probe_id: String,
    input_sha256: String,
    expected_result_sha256: String,
    observed_result_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hardware {
    device: String,
    operating_system: String,
    architecture: String,
    memory_bytes: u64,
    logical_processors: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    vram_bytes: Option<u64>,
}
fn strong(value: &str) -> bool {
    orange_protocol::valid_digest(value) && value.bytes().any(|c| c != b'0')
}
fn seconds(value: &str) -> Result<i64, AdminError> {
    let n = value.parse::<i64>().map_err(|_| AdminError::Invalid)?;
    if n < 0 || n.to_string() != value {
        return Err(AdminError::Invalid);
    }
    Ok(n)
}
/// One weight uses the measured file SHA; a sharded model binds every sorted weight path, size and digest.
fn weight(model: &ModelVersion) -> Result<String, AdminError> {
    let supported = |path: &str| match model.interface {
        ModelInterface::Text { .. } => path.ends_with(".gguf") || path.ends_with(".safetensors"),
        ModelInterface::Image { .. } => path.ends_with(".safetensors"),
        ModelInterface::Embedding { .. } => {
            path.ends_with(".onnx") || path.ends_with(".safetensors")
        }
        ModelInterface::Transcription { .. } => {
            path == "model.bin" || path.ends_with(".safetensors")
        }
    };
    let files: Vec<_> = model
        .artifacts
        .iter()
        .filter(|f| supported(&f.source_path))
        .map(|f| (&f.source_path, f.bytes, &f.sha256))
        .collect();
    match files.as_slice() {
        [] => Err(AdminError::Invalid),
        [(_, _, sha)] => Ok((*sha).clone()),
        _ => orange_protocol::request_digest("model-weight-set.v1", &files)
            .map_err(|_| AdminError::Invalid),
    }
}
impl Envelope {
    pub(super) fn check(
        &self,
        trust: &ModelReviewTrust,
        model: &ModelVersion,
        now: i64,
    ) -> Result<(String, i64, i64), AdminError> {
        let r = &self.report;
        let test = &r.measurement;
        let hw = &r.hardware;
        let issued = seconds(&r.issued_unix)?;
        let expires = seconds(&r.expires_unix)?;
        if r.version != 1
            || r.purpose != "model-qualification.v1"
            || r.environment != "dev"
            || r.model_version_id != model.model_version
            || r.manifest_sha256 != model.manifest_sha256().map_err(|_| AdminError::Invalid)?
            || r.model_sha256 != weight(model)?
            || !orange_protocol::valid_id(&r.execution_profile_id)
            || !strong(&r.execution_profile_digest)
            || issued > now
            || expires <= now
            || expires <= issued
            || r.license.identifier != model.source.license
            || r.license.source_revision != model.source.revision
            || r.license.permitted_use != "development-inference"
            || self.license_notice.trim().is_empty()
            || self.license_notice.len() > 131072
            || r.license.notice_sha256
                != hex::encode(Sha256::digest(self.license_notice.as_bytes()))
            || test.runtime_sha256
                != orange_protocol::request_digest("model-runtime-pin.v1", &model.runtime)
                    .map_err(|_| AdminError::Invalid)?
            || usize::try_from(test.verified_asset_count).ok() != Some(model.artifacts.len())
            || test.verified_asset_bytes != model.resources.artifact_bytes
            || !orange_protocol::valid_id(&test.probe_id)
            || !strong(&test.input_sha256)
            || !strong(&test.expected_result_sha256)
            || test.expected_result_sha256 != test.observed_result_sha256
            || hw.device != model.resources.device
            || !orange_protocol::valid_id(&hw.operating_system)
            || !orange_protocol::valid_id(&hw.architecture)
            || hw.memory_bytes == 0
            || hw.logical_processors == 0
            || model
                .resources
                .min_vram_bytes
                .is_some_and(|min| hw.vram_bytes.unwrap_or(0) < min)
            || model
                .resources
                .min_memory_bytes
                .is_some_and(|min| hw.memory_bytes < min)
        {
            return Err(AdminError::Invalid);
        }
        if self.signing_key_sha256 != trust.fingerprint {
            return Err(AdminError::Forbidden);
        }
        let id = orange_protocol::request_digest("model-qualification-report.v1", r)
            .map_err(|_| AdminError::Invalid)?;
        let signature = URL_SAFE_NO_PAD
            .decode(&self.signature)
            .map_err(|_| AdminError::Invalid)?;
        if signature.len() != 64 || URL_SAFE_NO_PAD.encode(&signature) != self.signature {
            return Err(AdminError::Invalid);
        }
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, trust.key)
            .verify(id.as_bytes(), &signature)
            .map_err(|_| AdminError::Forbidden)?;
        Ok((id, issued, expires))
    }
}
