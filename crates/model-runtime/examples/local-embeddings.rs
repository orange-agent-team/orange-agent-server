//! Read session credentials and input from a private launcher's stdin. Print only the batch result.
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use orange_model_runtime::{embeddings::PROFILE, llama::LlamaClient};
    use serde::Deserialize;
    use std::{io::Read, path::PathBuf};
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        socket: PathBuf,
        token: String,
        texts: Vec<String>,
    }
    let mut bytes = Vec::new();
    std::io::stdin().take(262145).read_to_end(&mut bytes)?;
    if bytes.len() > 262144 {
        return Err("input limit".into());
    }
    let input: Input = serde_json::from_slice(&bytes)?;
    let client = LlamaClient::private_socket(&input.socket, input.token, PROFILE.into(), 512)?;
    let (_authority, cancel) = tokio::sync::watch::channel(false);
    let result = client.embed(&input.texts, cancel).await?;
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("Private embedding socket is unsupported on this target");
    std::process::exit(1);
}
