//! Fetch an identical pinned public artifact from an explicitly selected official
//! distribution. No automatic mirror fallback or floating revision is allowed.
use orange_model_runtime::cache::{ArtifactSpec, ModelCache};
use serde::Deserialize;
use std::{env, error::Error, io::Read, path::PathBuf};
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
    #[serde(default)]
    official_alternates: Alternates,
}
#[derive(Default, Deserialize)]
struct Alternates {
    modelscope: Option<Alternate>,
}
#[derive(Deserialize)]
struct Alternate {
    repository: String,
    revision: String,
    bytes: u64,
    sha256: String,
}
fn fixed_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit())
}
fn source_url(
    model: &Model,
    source: &str,
) -> Result<(reqwest::Url, Vec<&'static str>), Box<dyn Error>> {
    let parts: Vec<&str> = model.repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        || !fixed_revision(&model.revision)
        || model.file.is_empty()
        || !model
            .file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        || !model.file.ends_with(".gguf")
    {
        return Err("Invalid pinned artifact manifest".into());
    }
    match source {
        "huggingface" => {
            let mut url = reqwest::Url::parse("https://huggingface.co/")?;
            url.path_segments_mut()
                .map_err(|_| "invalid base URL")?
                .extend([parts[0], parts[1], "resolve", &model.revision, &model.file]);
            Ok((url, vec!["huggingface.co", "us.aws.cdn.hf.co"]))
        }
        "modelscope" => {
            let alternate = model
                .official_alternates
                .modelscope
                .as_ref()
                .ok_or("No verified ModelScope source in pinned manifest")?;
            if alternate.repository != model.repository
                || alternate.bytes != model.bytes
                || alternate.sha256 != model.sha256
                || !fixed_revision(&alternate.revision)
            {
                return Err(
                    "Official alternate must identify the exact same immutable artifact".into(),
                );
            }
            let mut url = reqwest::Url::parse("https://modelscope.cn/")?;
            url.path_segments_mut()
                .map_err(|_| "invalid base URL")?
                .extend(["api", "v1", "models", parts[0], parts[1], "repo"]);
            url.query_pairs_mut()
                .append_pair("Revision", &alternate.revision)
                .append_pair("FilePath", &model.file);
            // Observed from the pinned publisher API's HTTPS redirect, not a
            // wildcard CDN/mirror allowance. Full content identity is unchanged.
            Ok((url, vec!["modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"]))
        }
        _ => Err("Unsupported official distribution selector".into()),
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("Usage: model-fetch <pinned-manifest.json> <private-cache-directory> [--source=huggingface|modelscope]".into());
    }
    let source = match args.get(2) {
        None => "huggingface",
        Some(value) => value
            .strip_prefix("--source=")
            .ok_or("Expected --source selector")?,
    };
    let file = std::fs::File::open(&args[0])?;
    if !file.metadata()?.is_file() {
        return Err("Regular manifest file required".into());
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err("Manifest size limit".into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    let model = manifest.model;
    let (url, hosts) = source_url(&model, source)?;
    let spec = ArtifactSpec::new(url.as_str(), &model.sha256, model.bytes, &hosts, b"GGUF")?;
    let cache = ModelCache::new(&PathBuf::from(&args[1]), 4 * 1024 * 1024 * 1024)?;
    let artifact = cache.ensure(&spec).await?;
    println!(
        "{}",
        serde_json::json!({"sha256":artifact.sha256(),"bytes":artifact.bytes(),"cache_hit":artifact.is_cached(),"verified":true,"official_source":source})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pinned() -> Model {
        serde_json::from_str::<Manifest>(include_str!("../../../../config/local-model.lock.json"))
            .expect("pinned manifest")
            .model
    }
    #[test]
    fn alternate_is_explicit_fixed_and_content_identical() {
        let model = pinned();
        let (url, hosts) = source_url(&model, "modelscope").expect("official alternate");
        assert_eq!(url.host_str(), Some("modelscope.cn"));
        assert_eq!(
            url.path(),
            "/api/v1/models/Qwen/Qwen2.5-0.5B-Instruct-GGUF/repo"
        );
        assert_eq!(hosts, vec!["modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"]);
        assert!(
            url.query_pairs().any(|(key, value)| key == "Revision"
                && value == "2e50b77b0eee3083842019e257b74854323d880a")
        );
        assert!(source_url(&model, "unverified-mirror").is_err());
        assert_eq!(
            source_url(&model, "huggingface")
                .expect("original")
                .0
                .host_str(),
            Some("huggingface.co")
        );
    }
    #[test]
    fn alternate_cannot_change_hash_size_owner_or_use_floating_revision() {
        for case in 0..4 {
            let mut model = pinned();
            let alt = model
                .official_alternates
                .modelscope
                .as_mut()
                .expect("alternate");
            match case {
                0 => alt.sha256 = "0".repeat(64),
                1 => alt.bytes += 1,
                2 => alt.repository = "another/publisher".into(),
                _ => alt.revision = "master".into(),
            }
            assert!(source_url(&model, "modelscope").is_err());
        }
        let mut model = pinned();
        model.official_alternates.modelscope = None;
        assert!(source_url(&model, "modelscope").is_err());
    }
}
