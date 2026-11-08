//! Trusted local developer CLI. Never auto-approves tool requests.
use orange_agent_runtime::{
    Journal,
    engine::{GatewayFailure, ModelFuture, ModelGateway, RunConfig},
    workspace::WorkspaceGrant,
};
use orange_model_runtime::llama::LlamaClient;
use orange_protocol::model::{GenerateRequest, Message, Role};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use tokio::sync::watch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    run: RunConfig,
    grant_id: String,
    workspace: PathBuf,
    journal: PathBuf,
    private_roots: Vec<PathBuf>,
    #[serde(default)]
    model_endpoint: Option<String>,
    #[serde(default)]
    model_socket: Option<PathBuf>,
    model_key_file: PathBuf,
    context_tokens: u32,
    prompt: String,
    #[serde(default)]
    user_questions: bool,
}
struct LocalModel(LlamaClient);
impl ModelGateway for LocalModel {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            let candidate = self
                .0
                .generate(request, cancel)
                .await
                .map_err(|_| GatewayFailure)?;
            for call in &candidate.tool_calls {
                let valid = if call.name == "ask_user" {
                    request.tools.iter().any(|t| t.name == "ask_user")
                        && orange_protocol::questions::AskUser::parse(&call.arguments).is_some()
                } else {
                    orange_agent_runtime::workspace::WorkspaceRequest::parse(
                        &call.name,
                        &call.arguments,
                    )
                    .is_ok()
                };
                if !valid {
                    // Shape-only diagnostics: never log model text, argument
                    // values, unknown field names or identifiers supplied by it.
                    let (name, known): (&str, &[&str]) = match call.name.as_str() {
                        "read_file" => (
                            "read_file",
                            &[
                                "path",
                                "start_byte",
                                "max_bytes",
                                "start_line",
                                "end_line",
                                "expected_sha256",
                            ],
                        ),
                        "list_directory" => ("list_directory", &["path", "cursor", "limit"]),
                        "search_files" => ("search_files", &["path", "glob", "limit"]),
                        "search_text" => (
                            "search_text",
                            &[
                                "path",
                                "query",
                                "glob",
                                "limit",
                                "regular_expression",
                                "case_sensitive",
                            ],
                        ),
                        "ask_user" => ("ask_user", &["questions"]),
                        _ => ("unavailable", &[]),
                    };
                    let object = call.arguments.as_object();
                    let present: Vec<&str> = known
                        .iter()
                        .copied()
                        .filter(|key| object.is_some_and(|value| value.contains_key(*key)))
                        .collect();
                    eprintln!("model_tool_known_fields={present:?}");
                    eprintln!(
                        "model_tool_argument_rejected tool={} object={} fields={} nulls={} empty_strings={} unexpected_fields={}",
                        name,
                        object.is_some(),
                        object.map_or(0, |v| v.len()),
                        object.map_or(0, |v| v.values().filter(|v| v.is_null()).count()),
                        object.map_or(0, |v| v
                            .values()
                            .filter(|v| v.as_str().is_some_and(str::is_empty))
                            .count()),
                        object.map_or(0, |v| v
                            .keys()
                            .filter(|key| !known.contains(&key.as_str()))
                            .count())
                    );
                }
            }
            Ok(candidate)
        })
    }
}
fn private_read(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 256 * 1024 {
        return Err("private regular input file required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err("private input permissions required (0600)".into());
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 256 * 1024 {
        return Err("input exceeds limit".into());
    }
    Ok(bytes)
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        return Err(
            "usage: local-agent <private-config.json> <start|show|step|approve OP_ID|answer|cancel>; answer reads bounded AnswerUser JSON from stdin"
                .into(),
        );
    }
    let config: Config = serde_json::from_slice(&private_read(Path::new(&args[1]))?)?;
    if config.model_endpoint.is_some() == config.model_socket.is_some() {
        return Err("Exactly one local model transport is required".into());
    }
    let mut private = config.private_roots;
    private.push(
        config
            .journal
            .parent()
            .ok_or("journal directory required")?
            .to_owned(),
    );
    private.push(
        config
            .model_key_file
            .parent()
            .ok_or("key directory required")?
            .to_owned(),
    );
    let grant = WorkspaceGrant::open(
        &config.run.owner,
        &config.grant_id,
        &config.run.policy,
        &config.workspace,
        &private,
    )?;
    if args[2] == "grant-info" {
        println!("{}", serde_json::json!({"environment":grant.environment()}));
        return Ok(());
    }
    if config.run.environment != grant.environment() {
        return Err(format!(
            "grant identity differs; expected environment {}",
            grant.environment()
        )
        .into());
    }
    // Read user data before acquiring the exclusive Journal owner lock.
    let answer_input = if args[2] == "answer" {
        if args.len() != 3 {
            return Err("answer data must use stdin, not process arguments".into());
        }
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(128 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 128 * 1024 {
            return Err("answer input exceeds bound".into());
        }
        Some(
            serde_json::from_slice::<orange_protocol::questions::AnswerUser>(&bytes)
                .map_err(|_| "invalid answer request")?,
        )
    } else {
        None
    };
    let mut journal = Journal::open(&config.journal)?;
    let view = match args[2].as_str() {
        "start" => {
            let messages = vec![Message::plain(Role::User, config.prompt)];
            if config.user_questions {
                journal.start_agent_interactive(&config.run, messages, &grant)?
            } else {
                journal.start_agent(&config.run, messages, &grant)?
            }
        }
        "show" => {
            journal
                .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
                .await?
        }
        "answer" => {
            let answer = answer_input.as_ref().ok_or("answer input required")?;
            let (sender, receiver) = watch::channel(false);
            let signal = tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = sender.send(true);
            });
            let result = journal
                .answer_agent_question(
                    &config.run.owner,
                    &config.run.run_id,
                    answer,
                    &grant,
                    receiver,
                )
                .await;
            signal.abort();
            result?
        }
        "approve" => {
            let id = args
                .get(3)
                .ok_or("operation ID required; inspect show before approval")?;
            let seconds = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            )?;
            journal.approve_agent_tool(
                &config.run.owner,
                &config.run.run_id,
                id,
                (seconds + 60).min(config.run.deadline_unix),
                &grant,
            )?;
            journal
                .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
                .await?
        }
        "cancel" => {
            let view = journal.cancel_agent(&config.run.owner, &config.run.run_id, &grant)?;
            grant.wait_drained().await;
            view
        }
        "step" => {
            let key = String::from_utf8(private_read(&config.model_key_file)?)?;
            let client = match (&config.model_endpoint, &config.model_socket) {
                (Some(endpoint), None) => LlamaClient::new(
                    endpoint,
                    key.trim().into(),
                    config.run.model_version.clone(),
                    config.context_tokens,
                )?,
                #[cfg(unix)]
                (None, Some(socket)) => LlamaClient::private_socket(
                    socket,
                    key.trim().into(),
                    config.run.model_version.clone(),
                    config.context_tokens,
                )?,
                _ => return Err("Local model transport unavailable on this target".into()),
            };
            let model = LocalModel(client);
            let (sender, receiver) = watch::channel(false);
            let signal = tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = sender.send(true);
            });
            let result = journal
                .advance_agent(
                    &config.run.owner,
                    &config.run.run_id,
                    &grant,
                    &model,
                    receiver,
                )
                .await;
            signal.abort();
            let view = result?;
            grant.wait_drained().await;
            view
        }
        _ => return Err("unknown command".into()),
    };
    let view =
        if view.final_text.is_some() || !view.pending.is_empty() || view.user_question.is_some() {
            journal
                .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
                .await?
        } else {
            view
        };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}
