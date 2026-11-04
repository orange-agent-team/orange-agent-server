//! User-mode CPU supervisor. The private control pipe is owned by its caller;
//! EOF, malformed input or missing heartbeat causes explicit child shutdown.
#[cfg(unix)]
mod unix {
    use orange_model_runtime::supervisor::{CpuProfile, ManagedCpuModel, ProcessRecord};
    use serde::Deserialize;
    use std::{
        io::{Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::PathBuf,
        time::Duration,
    };
    use tokio::io::AsyncReadExt;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        protocol: u32,
        runtime_directory: PathBuf,
        executable: PathBuf,
        executable_sha256: String,
        model: PathBuf,
        model_sha256: String,
        model_bytes: u64,
        model_version: String,
        context_tokens: u32,
        startup_timeout_seconds: u64,
    }
    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() != 1 {
            return Err("usage: orange-model-host <private-profile.json>".into());
        }
        let path = PathBuf::from(&args[0]);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || metadata.len() > 16 * 1024
            || std::fs::metadata(path.parent().ok_or("private parent required")?)?.mode() & 0o077
                != 0
        {
            return Err("private bounded profile required".into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Err("profile limit".into());
        }
        let config: Config = serde_json::from_slice(&bytes)?;
        if config.protocol != 1 {
            return Err("unsupported control protocol".into());
        }
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(run(config));
        // Tokio stdin uses blocking I/O. Never wait forever for an abandoned
        // partial control line after the model process has already been stopped.
        runtime.shutdown_timeout(Duration::from_secs(1));
        result
    }
    fn report(record: &ProcessRecord) -> std::io::Result<()> {
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(
            &mut output,
            &serde_json::json!({"protocol":1,"record":record}),
        )?;
        output.write_all(b"\n")?;
        output.flush()
    }
    async fn command(input: &mut tokio::io::Stdin) -> Result<Option<Vec<u8>>, std::io::Error> {
        let mut value = Vec::new();
        let mut byte = [0];
        loop {
            if input.read(&mut byte).await? == 0 {
                return if value.is_empty() {
                    Ok(None)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "partial control frame",
                    ))
                };
            }
            value.push(byte[0]);
            if value.len() > 32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "control frame limit",
                ));
            }
            if byte[0] == b'\n' {
                return Ok(Some(value));
            }
        }
    }
    async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let mut control = tokio::spawn(async move {
            let mut input = tokio::io::stdin();
            let failed = loop {
                match tokio::time::timeout(Duration::from_secs(30), command(&mut input)).await {
                    Ok(Ok(Some(frame))) if frame == b"ping\n" => {}
                    Ok(Ok(Some(frame))) if frame == b"shutdown\n" => break false,
                    Ok(Ok(None)) => break false,
                    _ => break true,
                }
            };
            let _ = cancel.send(true);
            failed
        });
        let launched = ManagedCpuModel::launch_with_cancel(
            &config.runtime_directory,
            CpuProfile {
                executable: config.executable,
                executable_sha256: config.executable_sha256,
                model: config.model,
                model_sha256: config.model_sha256,
                model_bytes: config.model_bytes,
                model_version: config.model_version,
                context_tokens: config.context_tokens,
                startup_timeout: Duration::from_secs(config.startup_timeout_seconds),
            },
            receiver,
        )
        .await;
        let mut process = match launched {
            Ok(process) => process,
            Err(orange_model_runtime::supervisor::ProcessError::Cancelled) => {
                if control.await? {
                    return Err("invalid control during startup".into());
                }
                return Ok(());
            }
            Err(error) => {
                control.abort();
                return Err(error.into());
            }
        };
        if report(process.record()).is_err() {
            process.shutdown().await?;
            control.abort();
            return Ok(());
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let failed = loop {
            tokio::select! {
                result=&mut control=>break result?,
                _=tick.tick()=>{
                    let record=process.poll_status()?;
                    if record.phase=="stopped"{control.abort();let _=report(&record);return Err("backend exited".into());}
                }
            }
        };
        process.shutdown().await?;
        if let Err(error) = report(process.record())
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
        if failed {
            return Err("control channel ended without a valid frame".into());
        }
        Ok(())
    }
}
#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::main()
}
#[cfg(not(unix))]
fn main() {
    eprintln!("CPU host IPC for this platform is not implemented");
    std::process::exit(2);
}
