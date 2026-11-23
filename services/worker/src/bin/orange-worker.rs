//! Local development CPU Worker; never installs a background service implicitly.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    if command.as_deref() == Some(std::ffi::OsStr::new("redeem-enrollment")) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let config = args
                .next()
                .ok_or("private redemption configuration required")?;
            if args.next().is_some() {
                return Err("expected one private redemption configuration".into());
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let installation = runtime.block_on(orange_worker::enrollment::redemption::redeem(
                std::path::Path::new(&config),
            ))?;
            serde_json::to_writer(std::io::stdout().lock(), &installation)?;
            println!();
            return Ok(());
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err("device key storage for this operating system is unavailable".into());
    }
    let handoff = command.as_deref() == Some(std::ffi::OsStr::new("prepare-enrollment-handoff"));
    if handoff || command.as_deref() == Some(std::ffi::OsStr::new("prepare-enrollment")) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let directory = args.next().ok_or("private enrollment directory required")?;
            let id = args.next().ok_or("request UUID required")?;
            if args.next().is_some() {
                return Err("expected a private directory and a request UUID".into());
            }
            let id = id.to_str().ok_or("invalid request UUID")?;
            let path = std::path::Path::new(&directory);
            if handoff {
                let public = orange_worker::enrollment::redemption::prepare_handoff(path, id)
                    .map_err(|e| e.to_string())?;
                serde_json::to_writer(std::io::stdout().lock(), &public)?;
            } else {
                let csr = orange_worker::enrollment::prepare(path, id)?;
                serde_json::to_writer(std::io::stdout().lock(), &csr)?;
            }
            println!();
            return Ok(());
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err("device key storage for this operating system is unavailable".into());
    }
    if command.as_deref() == Some(std::ffi::OsStr::new("probe-wsl")) {
        if args.next().is_some() {
            return Err("usage: orange-worker probe-wsl".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let report = runtime.block_on(orange_worker::wsl_inventory::observe());
        serde_json::to_writer(std::io::stdout().lock(), &report)?;
        println!();
        if report.status != orange_worker::wsl_inventory::WslStatus::Observed {
            std::process::exit(2);
        }
        return Ok(());
    }
    if command.as_deref() == Some(std::ffi::OsStr::new("probe-gpu")) {
        if args.next().is_some() {
            return Err("usage: orange-worker probe-gpu".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let report = runtime.block_on(orange_worker::gpu_inventory::observe());
        serde_json::to_writer(std::io::stdout().lock(), &report)?;
        println!();
        if report.status != orange_worker::gpu_inventory::InventoryStatus::Observed {
            std::process::exit(2);
        }
        return Ok(());
    }
    serve()
}
#[cfg(unix)]
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}
#[cfg(unix)]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use tokio::signal::unix::{SignalKind, signal};
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: orange-worker <private-development-config.json>")?;
    if path == "probe-transcription" {
        let config = args
            .next()
            .ok_or("private ASR probe configuration required")?;
        if args.next().is_some() {
            return Err("one private ASR probe configuration required".into());
        }
        let (sender, stop) = tokio::sync::watch::channel(false);
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let handle = tokio::spawn(async move {
            tokio::select! {_=interrupt.recv()=>{},_=terminate.recv()=>{}}
            let _ = sender.send(true);
        });
        let receipt =
            orange_worker::host::probe_transcription(std::path::Path::new(&config), stop).await;
        handle.abort();
        serde_json::to_writer(std::io::stdout().lock(), &receipt?)?;
        println!();
        return Ok(());
    }
    if args.next().is_some() {
        return Err("one private configuration path required".into());
    }
    let config = orange_worker::host::WorkerConfig::read(std::path::Path::new(&path))?;
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let (events, mut receiver) = tokio::sync::mpsc::channel(32);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let signals = tokio::spawn(async move {
        tokio::select! {_=terminate.recv()=>{},_=interrupt.recv()=>{}}
        let _ = shutdown.send(true);
    });
    // Stdout is diagnostics only; it cannot block the owned resource lifecycle.
    let printer = tokio::task::spawn_blocking(move || {
        while let Some(event) = receiver.blocking_recv() {
            let mut out = std::io::stdout().lock();
            if serde_json::to_writer(&mut out, &event).is_err()
                || out.write_all(b"\n").is_err()
                || out.flush().is_err()
            {
                break;
            }
        }
    });
    let result = orange_worker::host::run(config, stop, events).await;
    signals.abort();
    // The diagnostic pipe is not resource authority; don't wait forever if its
    // reader stopped consuming. Model cleanup has already been awaited by run.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), printer).await;
    result.map_err(Into::into)
}
#[cfg(not(unix))]
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("This development Worker requires a verified Unix process backend");
    std::process::exit(2);
}
