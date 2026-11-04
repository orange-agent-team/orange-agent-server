//! Test/inspection utility, not a charging API. Token counts must come from the pinned tokenizer harness.
use orange_domain::{Credits, TextDelivery, TextQuote, split_fee};
use orange_model_runtime::metering::{RawToolOutput, project};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    process::ExitCode,
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    mode: String,
    text: String,
    tools: Vec<RawToolOutput>,
    input_units: Option<u64>,
    output_units: Option<u64>,
    output_limit: Option<u64>,
    delivery: Option<String>,
}
fn run() -> Result<Value, &'static str> {
    let mut raw = String::new();
    io::stdin()
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut raw)
        .map_err(|_| "INPUT_ERROR")?;
    if raw.len() > 2 * 1024 * 1024 {
        return Err("INPUT_TOO_LARGE");
    }
    let input: Input = serde_json::from_str(&raw).map_err(|_| "INVALID_INPUT")?;
    let projection = project(&input.text, &input.tools).map_err(|_| "INVALID_PROJECTION")?;
    if input.mode == "project" {
        return serde_json::to_value(projection).map_err(|_| "SERIALIZATION_ERROR");
    }
    if input.mode != "price" {
        return Err("INVALID_MODE");
    }
    let delivery = match input.delivery.as_deref() {
        Some("committed_success") => TextDelivery::CommittedSuccess,
        Some("cancelled_persisted_text") => TextDelivery::CancelledPersistedText,
        Some("uncommitted") => TextDelivery::Uncommitted,
        Some("failed") => TextDelivery::Failed,
        Some("superseded") => TextDelivery::Superseded,
        _ => return Err("INVALID_DELIVERY"),
    };
    let quote = TextQuote {
        input_units: input.input_units.ok_or("MISSING_UNITS")?,
        output_limit: input.output_limit.ok_or("MISSING_LIMIT")?,
        input_price: Credits::new(2000).map_err(|_| "PRICE_ERROR")?,
        output_price: Credits::new(6000).map_err(|_| "PRICE_ERROR")?,
    };
    let charge = quote
        .charge(
            delivery,
            input.output_units.ok_or("MISSING_UNITS")?,
            projection.has_tools(),
        )
        .map_err(|_| "DELIVERY_OR_LIMIT_REJECTED")?;
    let (fee, provider) = split_fee(charge, 2000).map_err(|_| "PRICE_ERROR")?;
    Ok(
        json!({"charge":charge.to_string(),"platform_fee":fee.to_string(),"provider_pending":provider.to_string(),"quote_maximum":quote.maximum().map_err(|_|"PRICE_ERROR")?.to_string()}),
    )
}
fn main() -> ExitCode {
    match run() {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(code) => {
            println!("{}", json!({"error":code}));
            ExitCode::from(2)
        }
    }
}
