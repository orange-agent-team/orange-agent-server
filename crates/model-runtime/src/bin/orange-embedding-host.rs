//! The local E04 host owns a private socket and supervises at most one E09 CPU runner.
#[cfg(unix)]
mod local {
    use axum::{
        Json, Router,
        extract::{DefaultBodyLimit, MatchedPath, Request, State, rejection::JsonRejection},
        http::{StatusCode, header},
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::post,
    };
    use orange_model_runtime::{
        ModelError,
        embeddings::{BatchResult, EmbeddingRequest, PROFILE},
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        io::{self, Write},
        path::PathBuf,
        process::Stdio,
        sync::Arc,
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
        process::Command,
        sync::Semaphore,
    };

    const RUNNER: &str =
        include_str!("../../../../backends/orange_inference_backends/embedding.py");
    const PIN: &str = include_str!("../../../../config/local-embedding.lock.json");
    #[derive(Clone)]
    struct Host {
        python: PathBuf,
        models: PathBuf,
        pin: Value,
        token_hash: [u8; 32],
        slot: Arc<Semaphore>,
    }
    struct Failure(StatusCode, &'static str);
    impl IntoResponse for Failure {
        fn into_response(self) -> Response {
            (
                self.0,
                Json(json!({"error":{"code":self.1,"message":self.1}})),
            )
                .into_response()
        }
    }
    fn unavailable() -> Failure {
        Failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "embedding_backend_unavailable",
        )
    }

    async fn authorize(State(host): State<Host>, req: Request, next: Next) -> Response {
        let digest = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .filter(|s| s.len() == 64)
            .map(|s| <[u8; 32]>::from(Sha256::digest(s.as_bytes())));
        if digest != Some(host.token_hash) {
            return Failure(StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
        next.run(req).await
    }

    const COUNT_ROUTE: &str = "/v1/embeddings/input_tokens";
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Output {
        Batch(BatchResult),
        Counts {
            model: &'static str,
            projection_digest: String,
            item_tokens: Vec<u64>,
            input_tokens: u64,
        },
    }
    #[derive(Deserialize)]
    #[serde(untagged, deny_unknown_fields)]
    enum RunnerResult {
        Complete {
            lengths: Vec<u64>,
            vectors: Vec<Vec<f32>>,
        },
        Counts {
            lengths: Vec<u64>,
        },
        Error {
            error: String,
        },
    }

    async fn run(host: Host, texts: Vec<String>, count_only: bool) -> Result<Output, Failure> {
        let count = texts.len();
        let projection_digest =
            orange_protocol::request_digest("embedding.input.v1", &(&host.pin, &texts))
                .map_err(|_| unavailable())?;
        let frame =
            serde_json::to_vec(&json!({"profile":host.pin,"texts":texts,"count_only":count_only}))
                .map_err(|_| unavailable())?;
        if frame.len() > 262144 {
            return Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large"));
        }
        let mut child = Command::new(&host.python)
            .args(["-I", "-B", "-c", RUNNER])
            .arg(host.models)
            .env_clear()
            .env("TOKENIZERS_PARALLELISM", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| unavailable())?;
        let work = async {
            let mut input = child.stdin.take().ok_or_else(unavailable)?;
            input.write_all(&frame).await.map_err(|_| unavailable())?;
            drop(input);
            const MAX: u64 = 1024 * 1024;
            let mut data = Vec::new();
            child
                .stdout
                .take()
                .ok_or_else(unavailable)?
                .take(MAX + 1)
                .read_to_end(&mut data)
                .await
                .map_err(|_| unavailable())?;
            if data.len() as u64 > MAX {
                return Err(unavailable());
            }
            if !child.wait().await.map_err(|_| unavailable())?.success() {
                return Err(unavailable());
            }
            match serde_json::from_slice(&data).map_err(|_| unavailable())? {
                RunnerResult::Complete { lengths, vectors } if !count_only => {
                    BatchResult::from_runner(vectors, &lengths, count)
                        .map(Output::Batch)
                        .map_err(|_| unavailable())
                }
                RunnerResult::Counts { lengths }
                    if count_only
                        && lengths.len() == count
                        && lengths.iter().all(|n| (1..=512).contains(n)) =>
                {
                    Ok(Output::Counts {
                        model: PROFILE,
                        input_tokens: lengths.iter().sum(),
                        item_tokens: lengths,
                        projection_digest,
                    })
                }
                RunnerResult::Error { error } if error == "text_too_long" => {
                    Err(Failure(StatusCode::UNPROCESSABLE_ENTITY, "text_too_long"))
                }
                _ => Err(unavailable()),
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(90), work)
            .await
            .unwrap_or_else(|_| Err(unavailable()));
        if result.is_err() && child.try_wait().map_err(|_| unavailable())?.is_none() {
            child.kill().await.map_err(|_| unavailable())?;
        }
        result
    }

    async fn embed(
        State(host): State<Host>,
        path: MatchedPath,
        body: Result<Json<EmbeddingRequest>, JsonRejection>,
    ) -> Result<Json<Output>, Failure> {
        let Json(request) = body.map_err(|e| Failure(e.status(), "invalid_embedding_request"))?;
        let texts = request.into_texts().map_err(|e| match e {
            ModelError::Limit => Failure(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large"),
            _ => Failure(StatusCode::BAD_REQUEST, "invalid_embedding_request"),
        })?;
        let permit = host
            .slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure(StatusCode::TOO_MANY_REQUESTS, "embedding_slot_busy"))?;
        // A disconnected HTTP client cannot return a still-running slot to the host.
        tokio::spawn(async move {
            let _permit = permit;
            run(host, texts, path.as_str() == COUNT_ROUTE).await
        })
        .await
        .map_err(|_| unavailable())?
        .map(Json)
    }

    pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
        let [python, models] = args.as_slice() else {
            return Err("Usage: orange-embedding-host <absolute locked venv python> <absolute model directory>".into());
        };
        if !python.is_absolute() || !python.is_file() || !models.is_absolute() || !models.is_dir() {
            return Err("Absolute local Python and model directory required".into());
        }
        let private = tempfile::Builder::new().prefix("orange-embed-").tempdir()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = private.path().join("engine.sock");
        let listener = UnixListener::bind(&socket)?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let host = Host {
            python: python.clone(),
            models: models.clone(),
            pin: serde_json::from_str(PIN)?,
            token_hash: Sha256::digest(token.as_bytes()).into(),
            slot: Arc::new(Semaphore::new(1)),
        };
        let app = Router::new()
            .route("/v1/embeddings", post(embed))
            .route(COUNT_ROUTE, post(embed))
            .layer(DefaultBodyLimit::max(256 * 1024))
            .layer(middleware::from_fn_with_state(host.clone(), authorize))
            .with_state(host.clone());
        // Read this ephemeral credential only over the private launcher pipe.
        println!(
            "{}",
            json!({"socket":socket,"token":token,"model":PROFILE,"phase":"listening"})
        );
        io::stdout().flush()?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let mut byte = [0; 1];
                let _ = tokio::io::stdin().read(&mut byte).await;
            })
            .await?;
        let _drained = host.slot.acquire().await?;
        Ok(())
    }
}
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    local::serve().await
}
#[cfg(not(unix))]
fn main() {
    eprintln!("Private embedding socket is unsupported on this target");
    std::process::exit(1);
}
