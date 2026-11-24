//! Immutable descriptors for local release inspection. Model admission and price authorization have separate authorities.
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, OpenOptions};
use orange_protocol::{
    canonical::parse_tool_arguments,
    model_versions::{ModelManifestReceipt, ModelVersion},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{self, Read},
    path::Path,
    sync::LazyLock,
};

pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const SCHEMA: &str = include_str!("../../../contracts/json-schema/model-version.v1.json");
const BUNDLED: &[(&str, &str)] = &[
    (
        "multilingual-e5-small-onnx-cpu-v1",
        include_str!("../model-manifests/multilingual-e5-small-onnx-cpu-v1.json"),
    ),
    (
        "qwen-local-test-v1",
        include_str!("../model-manifests/qwen-local-test-v1.json"),
    ),
    (
        "sdxl-base-1.0-euler-fp16-mps-v1",
        include_str!("../model-manifests/sdxl-base-1.0-euler-fp16-mps-v1.json"),
    ),
    (
        "whisper-small-ct2-int8-cpu-v1",
        include_str!("../model-manifests/whisper-small-ct2-int8-cpu-v1.json"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid model version descriptor")]
    Invalid,
    #[error("bundled model version catalogue is unavailable")]
    Catalogue,
    #[error("model artifact size, type, identity or SHA-256 differs")]
    Integrity,
    #[error("model version not found")]
    NotFound,
    #[error("model artifact filesystem operation failed")]
    Io(#[from] io::Error),
}
static VALIDATOR: LazyLock<Result<jsonschema::Validator, ManifestError>> = LazyLock::new(|| {
    let schema: serde_json::Value =
        serde_json::from_str(SCHEMA).map_err(|_| ManifestError::Catalogue)?;
    jsonschema::validator_for(&schema).map_err(|_| ManifestError::Catalogue)
});

#[derive(Debug)]
pub struct ValidatedManifest(ModelManifestReceipt);

fn public_strings(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(v) => v.bytes().all(|b| (32..=126).contains(&b)),
        serde_json::Value::Object(v) => v.values().all(public_strings),
        serde_json::Value::Array(v) => v.iter().all(public_strings),
        _ => true,
    }
}

fn portable_names<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    let mut paths = BTreeSet::new();
    for name in names {
        for part in name.split('/') {
            let base = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = base
                .strip_prefix("COM")
                .or_else(|| base.strip_prefix("LPT"));
            if part.ends_with('.')
                || matches!(base.as_str(), "CON" | "AUX" | "PRN" | "NUL")
                || device.is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
            {
                return false;
            }
        }
        if !paths.insert(name.to_ascii_lowercase()) {
            return false;
        }
    }
    paths.iter().all(|name| {
        name.match_indices('/')
            .all(|(i, _)| !paths.contains(&name[..i]))
    })
}

impl ValidatedManifest {
    pub fn parse(input: &str) -> Result<Self, ManifestError> {
        if input.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid);
        }
        let json = parse_tool_arguments(input).map_err(|_| ManifestError::Invalid)?;
        if !public_strings(&json)
            || !VALIDATOR
                .as_ref()
                .map_err(|_| ManifestError::Catalogue)?
                .is_valid(&json)
        {
            return Err(ManifestError::Invalid);
        }
        let manifest: ModelVersion =
            serde_json::from_value(json).map_err(|_| ManifestError::Invalid)?;
        let files = &manifest.artifacts;
        if !files
            .windows(2)
            .all(|pair| pair[0].source_path < pair[1].source_path)
            || !portable_names(files.iter().map(|f| f.source_path.as_str()))
            || !portable_names(files.iter().map(|f| f.cache_path.as_str()))
            || files.iter().try_fold(0u64, |n, f| n.checked_add(f.bytes))
                != Some(manifest.resources.artifact_bytes)
        {
            return Err(ManifestError::Invalid);
        }
        let manifest_sha256 = manifest
            .manifest_sha256()
            .map_err(|_| ManifestError::Invalid)?;
        Ok(Self(ModelManifestReceipt {
            manifest_sha256,
            manifest,
        }))
    }
    pub fn receipt(&self) -> &ModelManifestReceipt {
        &self.0
    }

    /// Hash the bytes observed under the operator's selected root. No file handle or execution authority is returned.
    pub fn verify_assets(&self, root: &Path) -> Result<(), ManifestError> {
        if !std::fs::symlink_metadata(root)?.file_type().is_dir() {
            return Err(ManifestError::Integrity);
        }
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        let mut buf = vec![0u8; 1024 * 1024];
        for asset in &self.0.manifest.artifacts {
            let mut input = open_artifact(&dir, &asset.cache_path)?;
            let before = input.metadata()?;
            if before.len() != asset.bytes {
                return Err(ManifestError::Integrity);
            }
            let mut limited = (&mut input).take(asset.bytes + 1);
            let mut digest = Sha256::new();
            let mut count = 0u64;
            loop {
                let read = limited.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                digest.update(&buf[..read]);
                count += read as u64;
            }
            let named = open_artifact(&dir, &asset.cache_path)?.metadata()?;
            if count != asset.bytes
                || hex::encode(digest.finalize()) != asset.sha256
                || named.len() != before.len()
                || named.dev() != before.dev()
                || named.ino() != before.ino()
                || named.modified()? != before.modified()?
            {
                return Err(ManifestError::Integrity);
            }
        }
        Ok(())
    }
}
fn open_artifact(root: &Dir, path: &str) -> Result<File, ManifestError> {
    let mut dir = root.try_clone()?;
    let mut names = path.split('/').peekable();
    while let Some(name) = names.next() {
        if names.peek().is_some() {
            dir = dir.open_dir_nofollow(name)?;
            continue;
        }
        if !dir.symlink_metadata(name)?.is_file() {
            return Err(ManifestError::Integrity);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
        }
        let file = dir.open_with(name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(ManifestError::Integrity);
        }
        return Ok(file);
    }
    Err(ManifestError::Invalid)
}
pub fn bundled() -> Result<&'static [ValidatedManifest], ManifestError> {
    static VERSIONS: LazyLock<Result<Vec<ValidatedManifest>, ManifestError>> =
        LazyLock::new(|| {
            BUNDLED
                .iter()
                .map(|(id, raw)| {
                    let item = ValidatedManifest::parse(raw)?;
                    if item.0.manifest.model_version != *id {
                        return Err(ManifestError::Catalogue);
                    }
                    Ok(item)
                })
                .collect()
        });
    VERSIONS.as_deref().map_err(|_| ManifestError::Catalogue)
}
pub fn find(id: &str) -> Result<&'static ValidatedManifest, ManifestError> {
    bundled()?
        .iter()
        .find(|v| v.0.manifest.model_version == id)
        .ok_or(ManifestError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn verifies_open_regular_files_and_detects_changed_bytes_and_links()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value: Value = serde_json::from_str(BUNDLED[1].1)?;
        let digest = hex::encode(Sha256::digest(b"test"));
        value["artifacts"] = json!([{"source_path":"model.gguf","cache_path":"weights/test.gguf","sha256":digest,"bytes":4}]);
        value["resources"]["artifact_bytes"] = json!(4);
        let model = ValidatedManifest::parse(&value.to_string())?;
        let dir = tempfile::tempdir()?;
        let weights = dir.path().join("weights");
        std::fs::create_dir(&weights)?;
        let file = weights.join("test.gguf");
        std::fs::write(&file, b"test")?;
        model.verify_assets(dir.path())?;
        std::fs::write(&file, b"tEst")?;
        assert!(matches!(
            model.verify_assets(dir.path()),
            Err(ManifestError::Integrity)
        ));
        std::fs::write(&file, b"test-long")?;
        assert!(model.verify_assets(dir.path()).is_err());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir()?;
            std::fs::write(outside.path().join("test.gguf"), b"test")?;
            std::fs::remove_file(&file)?;
            std::os::unix::fs::symlink(outside.path().join("test.gguf"), &file)?;
            assert!(
                model.verify_assets(dir.path()).is_err(),
                "linked file accepted"
            );
            std::fs::remove_file(&file)?;
            std::fs::remove_dir(&weights)?;
            std::os::unix::fs::symlink(outside.path(), &weights)?;
            assert!(
                model.verify_assets(dir.path()).is_err(),
                "linked parent accepted"
            );
        }
        Ok(())
    }
}
