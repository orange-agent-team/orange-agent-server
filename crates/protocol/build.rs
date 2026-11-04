fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "grpc")]
    {
        println!("cargo:rerun-if-env-changed=PROTOC");
        println!("cargo:rerun-if-env-changed=PATH");
        let compiler = std::env::var_os("PROTOC").unwrap_or_else(|| "protoc".into());
        let version = std::process::Command::new(compiler)
            .arg("--version")
            .output()?;
        if !version.status.success()
            || String::from_utf8(version.stdout)?.trim() != "libprotoc 36.2"
        {
            return Err("Orange protocol build requires pinned Protobuf compiler 36.2".into());
        }
        println!("cargo:rerun-if-changed=../../contracts/proto/worker.proto");
        println!("cargo:rerun-if-changed=../../contracts/proto/host.proto");
        tonic_prost_build::configure().compile_protos(
            &[
                "../../contracts/proto/worker.proto",
                "../../contracts/proto/host.proto",
            ],
            &["../../contracts/proto"],
        )?;
    }
    Ok(())
}
