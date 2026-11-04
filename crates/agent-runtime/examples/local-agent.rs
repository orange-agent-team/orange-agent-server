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
    model_endpoint: String,
    model_key_file: PathBuf,
    context_tokens: u32,
    prompt: String,
}
struct LocalModel(LlamaClient);
impl ModelGateway for LocalModel {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            self.0
                .generate(request, cancel)
                .await
                .map_err(|_| GatewayFailure)
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
            "usage: local-agent <private-config.json> <start|show|step|approve OP_ID|cancel>"
                .into(),
        );
    }
    let config: Config = serde_json::from_slice(&private_read(Path::new(&args[1]))?)?;
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
    let mut journal = Journal::open(&config.journal)?;
    let view = match args[2].as_str() {
        "start" => journal.start_agent(
            &config.run,
            vec![Message::plain(Role::User, config.prompt)],
            &grant,
        )?,
        "show" => journal.agent_view(&config.run.owner, &config.run.run_id)?,
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
            journal.agent_view(&config.run.owner, &config.run.run_id)?
        }
        "cancel" => {
            let view = journal.cancel_agent(&config.run.owner, &config.run.run_id, &grant)?;
            grant.wait_drained().await;
            view
        }
        "step" => {
            let key = String::from_utf8(private_read(&config.model_key_file)?)?;
            let model = LocalModel(LlamaClient::new(
                &config.model_endpoint,
                key.trim().into(),
                config.run.model_version.clone(),
                config.context_tokens,
            )?);
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
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}
