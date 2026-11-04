use orange_model_runtime::{GenerateRequest, Message, ModelError, Role, llama::LlamaClient};
use tokio::sync::watch;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(std::env::var("ORANGE_LOCAL_MODEL_CONFIG")?)?)?;
    let key = config["api_key"].as_str().ok_or("key")?.to_owned();
    #[cfg(unix)]
    let client = if let Some(socket) = config["socket_path"].as_str() {
        LlamaClient::private_socket(
            std::path::Path::new(socket),
            key,
            "qwen-local-test-v1".into(),
            2048,
        )?
    } else {
        LlamaClient::new(
            config["endpoint"].as_str().ok_or("endpoint")?,
            key,
            "qwen-local-test-v1".into(),
            2048,
        )?
    };
    #[cfg(not(unix))]
    let client = {
        if config.get("socket_path").is_some() {
            return Err("Private Unix IPC unavailable on this target".into());
        }
        LlamaClient::new(
            config["endpoint"].as_str().ok_or("endpoint")?,
            key,
            "qwen-local-test-v1".into(),
            2048,
        )?
    };
    let request = GenerateRequest {
        invocation_id: "smoke-invocation".into(),
        model_version: "qwen-local-test-v1".into(),
        messages: vec![Message {
            role: Role::User,
            content: "Reply with exactly the word Orange and nothing else.".into(),
            tool_calls: vec![],
            tool_call_id: None,
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
    // The successful generation has already dropped its receiver. Persist the
    // cancellation value before creating the next receiver; send() would fail
    // with no listeners and would not establish this pre-dispatch test state.
    cancel_tx.send_replace(true);
    if !matches!(
        client.generate(&request, cancel_tx.subscribe()).await,
        Err(ModelError::Cancelled)
    ) {
        return Err("cancelled request dispatched".into());
    }
    println!("PASS context bound and pre-dispatch cancellation; no GPU reclamation claim");
    Ok(())
}
