//! Fixed-profile local llama.cpp adapter. No arbitrary URL, remote media, tool execution or paid calls.
use crate::{
    GenerateRequest, ModelError, ResponseCandidate,
    stream::{ChatAssembler, SseDecoder},
};
use reqwest::{Client, Response, Url};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;

pub struct LlamaClient {
    client: Client,
    endpoint: Url,
    key: String,
    model: String,
    context_tokens: u32,
}
impl LlamaClient {
    pub fn new(
        endpoint: &str,
        key: String,
        model: String,
        context_tokens: u32,
    ) -> Result<Self, ModelError> {
        let url = Url::parse(endpoint).map_err(|_| ModelError::Invalid)?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.path() != "/"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || key.len() < 32
            || !orange_protocol::valid_id(&model)
            || !(256..=32768).contains(&context_tokens)
        {
            return Err(ModelError::Invalid);
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ModelError::Unavailable)?;
        Ok(Self {
            client,
            endpoint: url,
            key,
            model,
            context_tokens,
        })
    }
    fn url(&self, path: &str) -> Result<Url, ModelError> {
        self.endpoint.join(path).map_err(|_| ModelError::Invalid)
    }
    async fn json(&self, path: &str, body: Value) -> Result<Value, ModelError> {
        let response = self
            .client
            .post(self.url(path)?)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|_| ModelError::Unavailable)?;
        serde_json::from_slice(&bounded(response).await?).map_err(|_| ModelError::Protocol)
    }
    fn request(&self, request: &GenerateRequest, stream: bool) -> Result<Value, ModelError> {
        request.validate()?;
        if request.model_version != self.model {
            return Err(ModelError::Invalid);
        }
        let mut body = json!({"model":self.model,"messages":request.messages,"max_tokens":request.max_output_tokens,
            "n":1,"seed":42,"temperature":0,"stream":stream,"cache_prompt":false});
        if stream {
            body["stream_options"] = json!({"include_usage":true});
        }
        if !request.tools.is_empty() {
            body["tools"]=Value::Array(request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}})).collect());
        }
        Ok(body)
    }
    /// Count the actual fixed model/template projection, not a client-provided hint.
    pub async fn input_tokens(&self, request: &GenerateRequest) -> Result<u64, ModelError> {
        let value = self
            .json(
                "v1/chat/completions/input_tokens",
                self.request(request, false)?,
            )
            .await?;
        let count = value["input_tokens"].as_u64().ok_or(ModelError::Protocol)?;
        if count
            .checked_add(u64::from(request.max_output_tokens))
            .is_none_or(|total| total > u64::from(self.context_tokens))
        {
            return Err(ModelError::Limit);
        }
        Ok(count)
    }
    /// Whole-content retokenization; caller chooses a versioned canonical projection.
    pub async fn tokenize(&self, content: &str) -> Result<Vec<i64>, ModelError> {
        if content.len() > 1024 * 1024 {
            return Err(ModelError::Limit);
        }
        let value = self
            .json(
                "tokenize",
                json!({"content":content,"add_special":false,"parse_special":false}),
            )
            .await?;
        value["tokens"]
            .as_array()
            .ok_or(ModelError::Protocol)?
            .iter()
            .map(|v| v.as_i64().filter(|&n| n >= 0).ok_or(ModelError::Protocol))
            .collect()
    }
    pub async fn generate(
        &self,
        request: &GenerateRequest,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<ResponseCandidate, ModelError> {
        if *cancellation.borrow() {
            return Err(ModelError::Cancelled);
        }
        // Dropped cancellation authority also fails closed. No network-level inference retries.
        tokio::select! {
            _=cancellation.changed()=>Err(ModelError::Cancelled),
            result=self.generate_inner(request)=>result,
        }
    }
    async fn generate_inner(
        &self,
        request: &GenerateRequest,
    ) -> Result<ResponseCandidate, ModelError> {
        self.input_tokens(request).await?;
        let mut response = self
            .client
            .post(self.url("v1/chat/completions")?)
            .bearer_auth(&self.key)
            .json(&self.request(request, true)?)
            .send()
            .await
            .map_err(|_| ModelError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ModelError::Unavailable);
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("text/event-stream"))
        {
            return Err(ModelError::Protocol);
        }
        let mut decoder = SseDecoder::default();
        let mut assembler = ChatAssembler::default();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ModelError::Unavailable)?
        {
            for event in decoder.push(&chunk)? {
                assembler.event(&event)?;
            }
        }
        decoder.finish()?;
        let candidate = assembler.finish()?;
        if candidate
            .native_usage
            .as_ref()
            .is_some_and(|usage| usage.output_tokens > u64::from(request.max_output_tokens))
        {
            return Err(ModelError::Limit);
        }
        if candidate
            .tool_calls
            .iter()
            .any(|call| !request.tools.iter().any(|tool| tool.name == call.name))
        {
            return Err(ModelError::Protocol);
        }
        Ok(candidate)
    }
}
async fn bounded(mut response: Response) -> Result<Vec<u8>, ModelError> {
    if !response.status().is_success() {
        return Err(ModelError::Unavailable);
    }
    const MAX: usize = 8 * 1024 * 1024;
    if response.content_length().is_some_and(|n| n > MAX as u64) {
        return Err(ModelError::Limit);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ModelError::Unavailable)?
    {
        if chunk.len() > MAX - bytes.len() {
            return Err(ModelError::Limit);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
