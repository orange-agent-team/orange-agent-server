fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "grpc")]
    {
        use sha2::{Digest, Sha256};
        use std::{fs, path::PathBuf};

        let lock: serde_json::Value = serde_json::from_str(include_str!("generation.lock.json"))?;
        println!("cargo:rerun-if-changed=generation.lock.json");
        println!("cargo:rerun-if-env-changed=PROTOC");
        println!("cargo:rerun-if-env-changed=PATH");
        let compiler = std::env::var_os("PROTOC").unwrap_or_else(|| "protoc".into());
        let version = std::process::Command::new(compiler)
            .arg("--version")
            .output()?;
        if !version.status.success()
            || Some(String::from_utf8(version.stdout)?.trim()) != lock["compiler"].as_str()
        {
            return Err("Orange protocol build requires pinned Protobuf compiler 36.2".into());
        }
        println!("cargo:rerun-if-changed=../../contracts/proto/worker.proto");
        println!("cargo:rerun-if-changed=../../contracts/proto/host.proto");
        let out =
            PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing Cargo output directory")?);
        let descriptor = out.join("orange.descriptor.pb");
        tonic_prost_build::configure()
            .file_descriptor_set_path(&descriptor)
            .compile_protos(
                &[
                    "../../contracts/proto/worker.proto",
                    "../../contracts/proto/host.proto",
                ],
                &["../../contracts/proto"],
            )?;
        for (name, expected) in [
            ("orange.descriptor.pb", &lock["descriptor_with_source_info"]),
            (
                "orange.worker.v1.rs",
                &lock["bindings"]["orange.worker.v1.rs"],
            ),
            ("orange.host.v1.rs", &lock["bindings"]["orange.host.v1.rs"]),
        ] {
            let actual = hex::encode(Sha256::digest(fs::read(out.join(name))?));
            if Some(actual.as_str()) != expected.as_str() {
                return Err(
                    format!("Generated {name} differs from the reviewed protocol lock").into(),
                );
            }
        }
    }
    Ok(())
}
