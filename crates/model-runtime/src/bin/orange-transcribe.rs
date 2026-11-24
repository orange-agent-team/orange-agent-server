//! Local MC-05 entry point with a caller-selected file, process cancellation and verified usage.
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use orange_model_runtime::transcription::{Client, read_audio};
    use std::io::Read;
    use std::path::Path;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--describe-input"] {
        serde_json::to_writer(
            std::io::stdout().lock(),
            &orange_model_runtime::transcription::input_contract()?,
        )?;
        println!();
        return Ok(());
    }
    if !(5..=7).contains(&args.len())
        || !matches!(args[0].as_str(), "measure" | "transcribe" | "confirm")
        || (args[0] == "measure" && args.len() != 5)
        || (args[0] == "transcribe" && args.len() > 6)
        || (args[0] == "confirm" && args.len() < 6)
    {
        return Err("Usage: orange-transcribe --describe-input | measure|transcribe|confirm <venv python> <verified models> <private slot directory> <audio file> [accepted measurement JSON for confirm] [language]".into());
    }
    let client = Client::new(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
    )?;
    let bytes = read_audio(Path::new(&args[4]))?;
    let (sender, cancel) = tokio::sync::watch::channel(false);
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sender.send(true);
    });
    let measured = if args[0] == "confirm" {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&args[5])?;
        if !file.metadata()?.is_file() {
            return Err("Expected regular measurement receipt".into());
        }
        let mut data = Vec::new();
        file.take(16385).read_to_end(&mut data)?;
        if data.len() > 16384 {
            return Err("Oversized measurement receipt".into());
        }
        serde_json::from_slice(&data)?
    } else {
        client.measure(&bytes, cancel.clone()).await?
    };
    let output = if args[0] == "measure" {
        measured
    } else {
        client
            .transcribe(
                &bytes,
                &measured,
                args.get(if args[0] == "confirm" { 6 } else { 5 })
                    .map(String::as_str),
                cancel,
            )
            .await?
    };
    signal.abort();
    serde_json::to_writer(std::io::stdout().lock(), &output)?;
    println!();
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("MC-05 CPU runner requires a qualified Unix host");
    std::process::exit(1);
}
