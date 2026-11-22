//! Offline operations report over an explicitly supplied pseudonymous event extract.
use orange_control_plane::market_metrics::{Snapshot, report};
use std::{fs::OpenOptions, io::Read};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let file = args
        .next()
        .ok_or("usage: orange-market-report SNAPSHOT.json")?;
    if args.next().is_some() {
        return Err("expected one snapshot file".into());
    }
    let mut bytes = Vec::new();
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let input = options.open(file)?;
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
        return Err("expected a regular snapshot file of at most 64 MiB".into());
    }
    input.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err("snapshot exceeds 64 MiB".into());
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "invalid marketplace input at line {}, column {}",
            e.line(),
            e.column()
        )
    })?;
    println!("{}", serde_json::to_string_pretty(&report(&snapshot)?)?);
    Ok(())
}
