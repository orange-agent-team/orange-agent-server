//! Fetch the pinned public Hugging Face integration artifact through the production cache primitives.
use orange_model_runtime::cache::{ArtifactSpec, ModelCache};
use serde::Deserialize;
use std::{env, error::Error, path::PathBuf};
#[derive(Deserialize)]
struct Manifest {
    model: Model,
}
#[derive(Deserialize)]
struct Model {
    repository: String,
    revision: String,
    file: String,
    bytes: u64,
    sha256: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("Usage: model-fetch <pinned-manifest.json> <private-cache-directory>".into());
    }
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let model = manifest.model;
    let parts: Vec<&str> = model.repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        || model.revision.len() != 40
        || !model.revision.bytes().all(|b| b.is_ascii_hexdigit())
        || model.file.is_empty()
        || !model
            .file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        || !model.file.ends_with(".gguf")
    {
        return Err("Invalid pinned Hugging Face manifest".into());
    }
    let mut url = reqwest::Url::parse("https://huggingface.co/")?;
    url.path_segments_mut()
        .map_err(|_| "invalid base URL")?
        .extend([parts[0], parts[1], "resolve", &model.revision, &model.file]);
    let spec = ArtifactSpec::new(
        url.as_str(),
        &model.sha256,
        model.bytes,
        &["huggingface.co", "us.aws.cdn.hf.co"],
        b"GGUF",
    )?;
    let cache = ModelCache::new(&PathBuf::from(&args[1]), 4 * 1024 * 1024 * 1024)?;
    let artifact = cache.ensure(&spec).await?;
    println!(
        "{}",
        serde_json::json!({"sha256":artifact.sha256(),"bytes":artifact.bytes(),"cache_hit":artifact.is_cached(),"verified":true})
    );
    Ok(())
}
