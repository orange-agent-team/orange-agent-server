//! Fixed-profile local llama.cpp adapter. No arbitrary URL, remote media, tool execution or paid calls.
use crate::adapter_contract::{CHAT_PATH, EMBEDDINGS_PATH, LocalAdapterSpec, RESPONSES_PATH};
use crate::{
    GenerateRequest, ModelError, ResponseCandidate,
    stream::{ChatAssembler, SseDecoder},
};
use reqwest::{Client, Response, Url};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalTextProtocol {
    #[default]
    ChatCompletions,
    Responses,
}
impl LocalTextProtocol {
    fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => CHAT_PATH,
            Self::Responses => RESPONSES_PATH,
        }
    }
}
pub struct LlamaClient {
    protocol: LocalTextProtocol,
    client: Client,
    endpoint: Url,
    key: String,
    model: String,
    context_tokens: u32,
}
impl LlamaClient {
    pub fn with_protocol(mut self, protocol: LocalTextProtocol) -> Self {
        self.protocol = protocol;
        self
    }
    /// Private filesystem IPC for an owned local backend, without a TCP port race.
    #[cfg(unix)]
    pub fn private_socket(
        socket: &std::path::Path,
        key: String,
        model: String,
        context_tokens: u32,
    ) -> Result<Self, ModelError> {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt},
        };
        let parent = socket.parent().ok_or(ModelError::Invalid)?;
        let parent_meta = std::fs::symlink_metadata(parent).map_err(|_| ModelError::Unavailable)?;
        let metadata = std::fs::symlink_metadata(socket).map_err(|_| ModelError::Unavailable)?;
        if !socket.is_absolute()
            || socket.as_os_str().as_bytes().len() > 100
            || !parent_meta.is_dir()
            || parent_meta.mode() & 0o077 != 0
            || !metadata.file_type().is_socket()
            || key.len() < 32
            || key.len() > 256
            || !key.bytes().all(|b| b.is_ascii_alphanumeric())
            || !orange_protocol::valid_id(&model)
            || !(256..=32768).contains(&context_tokens)
        {
            return Err(ModelError::Invalid);
        }
        let client = Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ModelError::Unavailable)?;
        Ok(Self {
            protocol: LocalTextProtocol::default(),
            client,
            endpoint: Url::parse("http://localhost/").map_err(|_| ModelError::Invalid)?,
            key,
            model,
            context_tokens,
        })
    }
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
            protocol: LocalTextProtocol::default(),
            client,
            endpoint: url,
            key,
            model,
            context_tokens,
        })
    }
    fn require_text(&self) -> Result<(), ModelError> {
        match crate::manifest::find(&self.model) {
            // Additional private text profiles (including the reviewed code model) have a separate
            // operator pin and select this text transport explicitly. They never gain a vector route.
            Err(crate::manifest::ManifestError::NotFound) => Ok(()),
            Ok(version) if matches!(LocalAdapterSpec::for_version(&version.receipt().manifest)?, LocalAdapterSpec::Text { context_tokens, .. } if self.context_tokens <= context_tokens) => {
                Ok(())
            }
            _ => Err(ModelError::Invalid),
        }
    }
    fn url(&self, path: &str) -> Result<Url, ModelError> {
        self.endpoint.join(path).map_err(|_| ModelError::Invalid)
    }
    pub async fn health(&self) -> Result<(), ModelError> {
        let response = self
            .client
            .get(self.url("health")?)
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|_| ModelError::Unavailable)?;
        let value: Value =
            serde_json::from_slice(&bounded(response).await?).map_err(|_| ModelError::Protocol)?;
        if value["status"] != "ok" {
            return Err(ModelError::Unavailable);
        }
        Ok(())
    }
    async fn post(&self, path: &str, body: &Value) -> Result<Response, ModelError> {
        self.client
            .post(self.url(path)?)
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .map_err(|_| ModelError::Unavailable)
    }
    async fn json(&self, path: &str, body: Value) -> Result<Value, ModelError> {
        let response = self.post(path, &body).await?;
        serde_json::from_slice(&bounded(response).await?).map_err(|_| ModelError::Protocol)
    }
    fn request(&self, request: &GenerateRequest, stream: bool) -> Result<Value, ModelError> {
        self.require_text()?;
        request.validate()?;
        if request.model_version != self.model {
            return Err(ModelError::Invalid);
        }
        if matches!(self.protocol, LocalTextProtocol::Responses) {
            return Ok(crate::responses::request_body(request, stream));
        }
        let messages: Vec<Value> = request.messages.iter().map(|message| {
            let mut value=json!({"role":message.role,"content":message.content});
            if let Some(id)=&message.tool_call_id {value["tool_call_id"]=json!(id);}
            if !message.tool_calls.is_empty() {
                value["tool_calls"]=Value::Array(message.tool_calls.iter().map(|call|json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})).collect());
            }
            value
        }).collect();
        let mut body = json!({"model":self.model,"messages":messages,"max_tokens":request.max_output_tokens,
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
                &format!("{}/input_tokens", self.protocol.path()),
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
    /// Text-only MP-03 route, restricted to the separately audited local embedding profile.
    pub async fn embed(
        &self,
        texts: &[String],
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<crate::embeddings::BatchResult, ModelError> {
        crate::embeddings::validate_texts(texts)?;
        if !matches!(
            LocalAdapterSpec::bundled(&self.model)?,
            LocalAdapterSpec::Embedding { .. }
        ) {
            return Err(ModelError::Invalid);
        }
        if *cancellation.borrow() {
            return Err(ModelError::Cancelled);
        }
        let body = json!({"model":self.model,"input":texts,"encoding_format":"float"});
        let fetch = async {
            let response = self.post(EMBEDDINGS_PATH, &body).await?;
            match response.status().as_u16() {
                400 => return Err(ModelError::Invalid),
                413 | 422 => return Err(ModelError::Limit),
                _ => {}
            }
            serde_json::from_slice::<crate::embeddings::BatchResult>(&bounded(response).await?)
                .map_err(|_| ModelError::Protocol)
        };
        let mut batch = tokio::select! {
            _=cancellation.changed()=>return Err(ModelError::Cancelled),
            result=fetch=>result?,
        };
        batch.verify(texts.len())?;
        Ok(batch)
    }
    /// Whole-content retokenization; caller chooses a versioned canonical projection.
    pub async fn tokenize(&self, content: &str) -> Result<Vec<i64>, ModelError> {
        self.require_text()?;
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
            .post(self.url(self.protocol.path())?)
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
        let mut chat = ChatAssembler::default();
        let mut responses = crate::responses::ResponsesAssembler::default();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ModelError::Unavailable)?
        {
            for event in decoder.push(&chunk)? {
                match self.protocol {
                    LocalTextProtocol::ChatCompletions => chat.event(&event)?,
                    LocalTextProtocol::Responses => responses.event(&event)?,
                }
            }
        }
        decoder.finish()?;
        let candidate = match self.protocol {
            LocalTextProtocol::ChatCompletions => chat.finish()?,
            LocalTextProtocol::Responses => {
                responses.finish(&self.model, request.max_output_tokens)?
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use orange_protocol::model::{Message, Role, ToolCall};
    #[tokio::test]
    async fn bundled_capabilities_reject_cross_modality_before_any_http_request() {
        let local = |model: &str, context| {
            LlamaClient::new("http://127.0.0.1:1/", "k".repeat(32), model.into(), context)
                .expect("local transport")
        };
        let (_, cancel) = watch::channel(false);
        let strings = vec!["query: sample".into()];
        for id in [
            "qwen-local-test-v1",
            "looks-like-an-embedding-model",
            "sdxl-base-1.0-euler-fp16-mps-v1",
        ] {
            assert!(matches!(
                local(id, 512).embed(&strings, cancel.clone()).await,
                Err(ModelError::Invalid)
            ));
        }
        for (id, ctx) in [
            (crate::embeddings::PROFILE, 512),
            ("whisper-small-ct2-int8-cpu-v1", 4096),
            ("qwen-local-test-v1", 8192),
        ] {
            let client = local(id, ctx);
            assert!(matches!(
                client.tokenize("sample").await,
                Err(ModelError::Invalid)
            ));
            let request = GenerateRequest {
                invocation_id: "one".into(),
                model_version: id.into(),
                max_output_tokens: 10,
                tools: vec![],
                messages: vec![Message::plain(Role::User, "sample".into())],
            };
            assert!(matches!(
                client.input_tokens(&request).await,
                Err(ModelError::Invalid)
            ));
        }
        let text = local("qwen-local-test-v1", 4096);
        let request = GenerateRequest {
            invocation_id: "privacy".into(),
            model_version: "qwen-local-test-v1".into(),
            max_output_tokens: 10,
            tools: vec![],
            messages: vec![Message::plain(Role::User, "sample".into())],
        };
        assert_eq!(
            text.request(&request, true)
                .expect("valid Chat Completions payload")["cache_prompt"],
            false
        );
        let body = text
            .with_protocol(LocalTextProtocol::Responses)
            .request(&request, true)
            .expect("valid Responses payload");
        assert_eq!(body["cache_prompt"], false);
        assert_eq!(body["store"], false);
        assert!(body.get("previous_response_id").is_none());
        assert!(
            local("private-reviewed-text-alias", 4096)
                .require_text()
                .is_ok()
        );
        assert!(matches!(
            LocalAdapterSpec::bundled(crate::embeddings::PROFILE),
            Ok(LocalAdapterSpec::Embedding {
                dimensions: 384,
                ..
            })
        ));
    }

    #[test]
    fn internal_tool_calls_map_to_native_function_history() {
        let client = LlamaClient::new(
            "http://127.0.0.1:44099/",
            "k".repeat(32),
            "local-model".into(),
            2048,
        )
        .expect("local client");
        let request = GenerateRequest {
            invocation_id: "inv".into(),
            model_version: "local-model".into(),
            max_output_tokens: 100,
            tools: vec![],
            messages: vec![
                Message::plain(Role::User, "read".into()),
                Message {
                    role: Role::Assistant,
                    content: "".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"note.txt"}),
                    }],
                    tool_call_id: None,
                },
                Message {
                    role: Role::Tool,
                    content: "result".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                },
            ],
        };
        let body = client.request(&request, true).expect("body");
        assert_eq!(
            body["messages"][1]["tool_calls"][0],
            json!({"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"note.txt\"}"}})
        );
        assert_eq!(body["messages"][2]["tool_call_id"], "call-1");
        assert_eq!(body["messages"][2]["role"], "tool");
        assert!(
            body["messages"][1]["tool_calls"][0]
                .get("arguments")
                .is_none()
        );
    }
}
