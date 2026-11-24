//! Offline operator interface to inspect, validate and measure model versions.
use orange_model_runtime::manifest::{self, ValidatedManifest};
use std::{
    io::{self, Read},
    path::Path,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let out = io::stdout().lock();
    match args.as_slice(){
        [op] if op=="list"=>serde_json::to_writer(out,&manifest::bundled()?.iter().map(|v|(&v.receipt().manifest.model_version,&v.receipt().manifest_sha256)).collect::<Vec<_>>())?,
        [op,id] if op=="show"=>serde_json::to_writer(out,manifest::find(id)?.receipt())?,
        [op] if op=="validate"=>{
            let mut text=String::new();
            io::stdin().lock().take(manifest::MAX_MANIFEST_BYTES as u64+1).read_to_string(&mut text)?;
            serde_json::to_writer(out,ValidatedManifest::parse(&text)?.receipt())?;
        }
        [op,id,root] if op=="verify"=>{
            let version=manifest::find(id)?;
            version.verify_assets(Path::new(root))?;
            let receipt=version.receipt();
            serde_json::to_writer(out,&serde_json::json!({"model_version":id,"manifest_sha256":receipt.manifest_sha256,
            "verified_files":receipt.manifest.artifacts.len(),"verified_bytes":receipt.manifest.resources.artifact_bytes}))?;
        }
        _=>return Err("usage: orange-model-manifest list | show VERSION | validate < manifest.json | verify VERSION CACHE_DIRECTORY".into())
    }
    println!();
    Ok(())
}
