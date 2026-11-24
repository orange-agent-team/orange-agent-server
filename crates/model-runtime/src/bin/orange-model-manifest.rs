//! Offline operator interface to inspect, validate and measure model versions.
use orange_model_runtime::manifest::{self, ValidatedManifest};
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    path::Path,
};

fn proposed_manifest() -> Result<ValidatedManifest, Box<dyn std::error::Error>> {
    let mut text = String::new();
    io::stdin()
        .lock()
        .take(manifest::MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    Ok(ValidatedManifest::parse(&text)?)
}

fn measured(version: &ValidatedManifest, cache: &str) -> Result<Value, manifest::ManifestError> {
    version.verify_assets(Path::new(cache))?;
    let receipt = version.receipt();
    Ok(json!({
        "model_version": receipt.manifest.model_version,
        "manifest_sha256": receipt.manifest_sha256,
        "verified_files": receipt.manifest.artifacts.len(),
        "verified_bytes": receipt.manifest.resources.artifact_bytes
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let output = match args.as_slice() {
        [op] if op == "list" => {
            let versions: Vec<_> = manifest::bundled()?
                .iter()
                .map(|v| (&v.receipt().manifest.model_version, &v.receipt().manifest_sha256))
                .collect();
            serde_json::to_value(versions)?
        }
        [op, id] if op == "show" => serde_json::to_value(manifest::find(id)?.receipt())?,
        [op] if op == "validate" => serde_json::to_value(proposed_manifest()?.receipt())?,
        [op, id, cache] if op == "verify" => measured(manifest::find(id)?, cache)?,
        [op, cache] if op == "verify-proposed" => measured(&proposed_manifest()?, cache)?,
        _ => return Err("usage: orange-model-manifest list | show VERSION | validate < FILE | verify VERSION CACHE | verify-proposed CACHE < FILE".into())
    };
    serde_json::to_writer(io::stdout().lock(), &output)?;
    println!();
    Ok(())
}
