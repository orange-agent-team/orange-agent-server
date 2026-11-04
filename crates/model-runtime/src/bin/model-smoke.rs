use orange_model_runtime::{GenerateRequest, Message, ModelError, Role, llama::LlamaClient};
use tokio::sync::watch;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(std::env::var("ORANGE_LOCAL_MODEL_CONFIG")?)?)?;
    let client = LlamaClient::new(
        config["endpoint"].as_str().ok_or("endpoint")?,
        config["api_key"].as_str().ok_or("key")?.into(),
        "qwen-local-test-v1".into(),
        2048,
    )?;
    let request = GenerateRequest {
        invocation_id: "smoke-invocation".into(),
        model_version: "qwen-local-test-v1".into(),
        messages: vec![Message {
            role: Role::User,
            content: "Reply with exactly the word Orange and nothing else.".into(),
        }],
        tools: vec![],
        max_output_tokens: 64,
    };
    let input = client.input_tokens(&request).await?;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let response = client.generate(&request, cancel_rx).await?;
    if response.text.trim().is_empty() || !response.tool_calls.is_empty() {
        return Err("actual text response required".into());
    }
    let native = response
        .native_usage
        .ok_or("actual native usage required")?;
    let tokens = client.tokenize(&response.text).await?;
    if tokens.is_empty()
        || native.input_tokens != input
        || native.output_tokens == 0
        || native.output_tokens > 64
    {
        return Err("model accounting evidence inconsistent".into());
    }
    println!(
        "PASS real local CPU inference: input_tokens={input}, native_output_tokens={}, retokenized_visible_output={}; response candidate only, not billed or host-committed",
        native.output_tokens,
        tokens.len()
    );
    let mut invalid = request.clone();
    invalid.max_output_tokens = 4096;
    if !matches!(client.input_tokens(&invalid).await, Err(ModelError::Limit)) {
        return Err("context overflow was not rejected".into());
    }
    cancel_tx.send(true)?;
    if !matches!(
        client.generate(&request, cancel_tx.subscribe()).await,
        Err(ModelError::Cancelled)
    ) {
        return Err("cancelled request dispatched".into());
    }
    println!("PASS context bound and pre-dispatch cancellation; no GPU reclamation claim");
    Ok(())
}
