//! Test peer for Dart interoperability. Each input is a JSON value representing an artifact reference.
use orange_protocol::marketplace::JobInputRef;
use serde_json::Value;
use std::io::{self, Read};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    io::stdin().take(1_048_577).read_to_end(&mut input)?;
    if input.len() > 1_048_576 {
        return Err("fixture corpus exceeds 1 MiB".into());
    }
    let cases: Vec<Value> = serde_json::from_slice(&input)?;
    let normalized: Vec<Option<JobInputRef>> = cases
        .into_iter()
        .map(|value| {
            let input = serde_json::from_value::<JobInputRef>(value).ok()?;
            input.pin().ok()?;
            Some(input)
        })
        .collect();
    serde_json::to_writer(io::stdout().lock(), &normalized)?;
    Ok(())
}
