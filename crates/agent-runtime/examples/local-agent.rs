//! Trusted local developer CLI. Never auto-approves tool requests.
#[path = "local-agent/verified_model.rs"]
mod verified_model;
use orange_agent_runtime::{
    Journal,
    engine::{CreationApproval, GatewayFailure, ModelFuture, ModelGateway, RunConfig},
    workspace::WorkspaceGrant,
};
use orange_model_runtime::llama::{LlamaClient, LocalTextProtocol};
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
    #[serde(default)]
    model_protocol: LocalTextProtocol,
    model_key_file: PathBuf,
    context_tokens: u32,
    prompt: String,
    #[serde(default)]
    skill: Option<orange_agent_runtime::skills::SkillPin>,
    #[serde(default)]
    skill_activation_revision: Option<i64>,
    #[serde(default)]
    user_questions: bool,
    #[serde(default)]
    creation_consent_id: Option<String>,
    #[serde(default)]
    editing_consent_id: Option<String>,
}
struct LocalModel(LlamaClient);
impl ModelGateway for LocalModel {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        let pending = self.generate_observed(request, cancel);
        Box::pin(async move { pending.await.map_err(|_| GatewayFailure) })
    }
    fn generate_observed<'a>(
        &'a self,
        request: &'a GenerateRequest,
        cancel: watch::Receiver<bool>,
    ) -> orange_agent_runtime::engine::ObservedModelFuture<'a> {
        Box::pin(async move {
            let candidate = self.0.generate(request, cancel).await.map_err(|error| {
                // ModelError has only fixed variants, no prompt, key or URL.
                eprintln!("model_gateway_rejected kind={error:?}");
                orange_protocol::model::GatewayIssue::from(error)
            })?;
            for call in &candidate.tool_calls {
                let valid = if call.name == "ask_user" {
                    request.tools.iter().any(|t| t.name == "ask_user")
                        && orange_protocol::questions::AskUser::parse(&call.arguments).is_some()
                } else {
                    request.tools.iter().any(|t| t.name == call.name)
                        && orange_agent_runtime::workspace::WorkspaceRequest::parse(
                            &call.name,
                            &call.arguments,
                        )
                        .is_ok()
                };
                if !valid {
                    // Shape-only diagnostics: never log model text, argument
                    // values, unknown field names or identifiers supplied by it.
                    let (name, known): (&str, &[&str]) = match call.name.as_str() {
                        "write_file" => ("write_file", &["path", "content"]),
                        "apply_patch" => ("apply_patch", &["path", "expected_sha256", "patch"]),
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
    let command=args.get(2).map(String::as_str).ok_or("usage: local-agent PRIVATE_CONFIG COMMAND; editing requires separate authorize-edit, preview-edit and approve-edit")?;
    let expected = match command {
        "skills" | "skill-binding" | "review-skill" | "model-observation" => 3,
        "install-skill" | "uninstall-skill" => 5,
        "enable-skill" | "disable-skill" => 4,
        "pending-edits" if args.len() == 4 => 4,
        "pending-edits" => 3,
        "start" | "show" | "status" | "step" | "answer" | "cancel" | "grant-info" => 3,
        "approve" | "authorize-create" | "revoke-create" | "authorize-edit" | "revoke-edit"
        | "preview-edit" | "inspect-edit" => 4,
        "approve-create" | "approve-edit" => 5,
        _ => return Err("unknown command".into()),
    };
    if args.len() != expected {
        return Err(
            "wrong command argument count; approval requires the exact displayed digest".into(),
        );
    }
    let config: Config = serde_json::from_slice(&private_read(Path::new(&args[1]))?)?;
    if matches!(
        command,
        "skills"
            | "skill-binding"
            | "enable-skill"
            | "disable-skill"
            | "review-skill"
            | "install-skill"
            | "uninstall-skill"
    ) {
        let mut journal = Journal::open(&config.journal)?;
        match command {
            "review-skill" | "install-skill" => {
                let mut bytes = Vec::new();
                std::io::stdin()
                    .take(orange_agent_runtime::skills::LOCAL_SKILL_MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if command == "review-skill" {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &journal.review_skill_package(&config.run.owner, &bytes)?
                        )?
                    );
                } else {
                    let revision = journal.install_skill_package(
                        &config.run.owner,
                        &bytes,
                        &args[3],
                        args[4].parse()?,
                    )?;
                    println!("{}", serde_json::json!({"installation_revision":revision}));
                }
            }
            "uninstall-skill" => {
                let pin = config
                    .skill
                    .as_ref()
                    .ok_or("Select the reviewed skill pin")?;
                let revision = journal.uninstall_skill_package(
                    &config.run.owner,
                    pin,
                    args[3].parse()?,
                    args[4].parse()?,
                )?;
                println!("{}", serde_json::json!({"installation_revision":revision}));
            }
            "skills" => println!(
                "{}",
                serde_json::to_string_pretty(&journal.skills(&config.run.owner)?)?
            ),
            "skill-binding" => println!(
                "{}",
                serde_json::to_string_pretty(
                    &journal.run_skill(&config.run.owner, &config.run.run_id)?
                )?
            ),
            _ => {
                let pin = config
                    .skill
                    .as_ref()
                    .ok_or("Select a catalog pin in the private configuration")?;
                let revision = journal.set_skill_enabled(
                    &config.run.owner,
                    pin,
                    args[3].parse()?,
                    command == "enable-skill",
                )?;
                println!(
                    "{}",
                    serde_json::json!({"pin":pin,"revision":revision,"enabled":command=="enable-skill"})
                );
            }
        }
        return Ok(());
    }
    if command == "model-observation" {
        let journal = Journal::open(&config.journal)?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &journal.latest_model_observation(&config.run.owner, &config.run.run_id)?
            )?
        );
        return Ok(());
    }
    if command == "status" {
        let journal = Journal::open(&config.journal)?;
        let view = journal.agent_view(&config.run.owner, &config.run.run_id)?;
        println!(
            "{}",
            serde_json::json!({"run_id":view.run_id,"phase":view.phase,"steps":view.steps,"invocations":view.invocations,"pending_operations":view.pending.len(),"awaiting_user":view.user_question.is_some(),"failure":view.failure})
        );
        return Ok(());
    }
    #[cfg(target_vendor = "apple")]
    if command == "pending-edits" {
        let journal = Journal::open(&config.journal)?;
        let ids = journal.agent_unresolved_patch_ids(
            &config.run.owner,
            &config.run.run_id,
            args.get(3).map(String::as_str),
            100,
        )?;
        let next = if ids.len() == 100 { ids.last() } else { None };
        println!(
            "{}",
            serde_json::json!({"operation_ids":ids,"next_before":next})
        );
        return Ok(());
    }
    #[cfg(not(target_vendor = "apple"))]
    if config.editing_consent_id.is_some()
        || matches!(
            command,
            "authorize-edit"
                | "revoke-edit"
                | "preview-edit"
                | "approve-edit"
                | "inspect-edit"
                | "pending-edits"
        )
    {
        return Err("editing is unavailable on this target".into());
    }
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
    // Configured exclusions may include readonly model assets, not writable
    // private state namespaces. WorkspaceGrant already rejects overlap with
    // every exclusion; creation must match that independently checked resource.
    if args[2] == "authorize-create" {
        if args.len() != 4 {
            return Err("explicit create consent request ID required".into());
        }
        let create = journal.authorize_file_creation(
            &config.run.owner,
            &args[3],
            &config.run.policy,
            &config.workspace,
        )?;
        if create.resource_identity() != grant.resource_identity() {
            create.request_revoke();
            journal.revoke_file_creation(
                &config.run.owner,
                create.consent_id(),
                &create.consent_version(),
            )?;
            return Err("workspace identity changed during create authorization".into());
        }
        println!(
            "{}",
            serde_json::to_string_pretty(
                &journal.creation_consent(&config.run.owner, create.consent_id())?
            )?
        );
        return Ok(());
    }
    if args[2] == "revoke-create" {
        if args.len() != 4 {
            return Err("exact consent version required".into());
        }
        let id = config
            .creation_consent_id
            .as_deref()
            .ok_or("explicit consent ID required")?;
        journal.revoke_file_creation(&config.run.owner, id, &args[3])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&journal.creation_consent(&config.run.owner, id)?)?
        );
        return Ok(());
    }
    #[cfg(target_vendor = "apple")]
    match command {
        "authorize-edit" => {
            let edit = journal.authorize_file_edit(
                &config.run.owner,
                &args[3],
                &config.run.policy,
                &config.workspace,
            )?;
            if edit.resource_identity() != grant.resource_identity() {
                edit.request_revoke();
                journal.revoke_file_edit(
                    &config.run.owner,
                    edit.consent_id(),
                    &edit
                        .consent_version()
                        .ok_or("durable consent version required")?,
                )?;
                return Err("workspace identity changed during edit authorization".into());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &journal.editing_consent(&config.run.owner, edit.consent_id())?
                )?
            );
            return Ok(());
        }
        "revoke-edit" => {
            let id = config
                .editing_consent_id
                .as_deref()
                .ok_or("explicit edit consent ID required")?;
            journal.revoke_file_edit(&config.run.owner, id, &args[3])?;
            println!(
                "{}",
                serde_json::to_string_pretty(&journal.editing_consent(&config.run.owner, id)?)?
            );
            return Ok(());
        }
        "inspect-edit" => {
            let observed = journal
                .inspect_patch_recovery(&config.run.owner, &args[3], &grant)
                .await?;
            println!("{}", serde_json::to_string_pretty(observed.snapshot()?)?);
            return Ok(());
        }
        _ => {}
    }
    let create = if matches!(
        command,
        "start" | "step" | "approve-create" | "preview-edit" | "approve-edit"
    ) {
        config
            .creation_consent_id
            .as_deref()
            .map(|id| journal.restore_file_creation(&config.run.owner, id))
            .transpose()?
    } else {
        None
    };
    #[cfg(target_vendor = "apple")]
    let edit = if matches!(command, "start" | "step" | "preview-edit" | "approve-edit") {
        config
            .editing_consent_id
            .as_deref()
            .map(|id| journal.restore_file_edit(&config.run.owner, id))
            .transpose()?
    } else {
        None
    };
    #[cfg(target_vendor = "apple")]
    if command == "preview-edit" || command == "approve-edit" {
        let edit = edit.as_ref().ok_or("explicit edit consent ID required")?;
        let authority = orange_agent_runtime::engine::EditingAuthority {
            read: &grant,
            create: create.as_ref(),
            edit,
        };
        if command == "preview-edit" {
            let preview = journal
                .agent_patch_preview(&config.run.owner, &config.run.run_id, &args[3], authority)
                .await?;
            println!("{}", serde_json::to_string_pretty(&preview)?);
            return Ok(());
        }
        let seconds = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        )?;
        journal
            .approve_agent_editing(
                &config.run.owner,
                &config.run.run_id,
                orange_agent_runtime::engine::EditingApproval {
                    operation_id: &args[3],
                    preview_digest: &args[4],
                    expires_at: (seconds + 60).min(config.run.deadline_unix),
                },
                authority,
            )
            .await?;
        let view = journal
            .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
            .await?;
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    if command == "start" && config.skill_activation_revision.is_some() && config.skill.is_none() {
        return Err("Activation revision requires a skill pin".into());
    }
    let view = match command {
        #[cfg(target_vendor = "apple")]
        "start"
            if config
                .skill
                .as_ref()
                .is_some_and(|p| p.id == "orange.reviewed-patch") =>
        {
            journal.start_agent_patch_skill(
                &config.run,
                config.skill.as_ref().expect("selected patch template"),
                config.skill_activation_revision,
                config.prompt,
                orange_agent_runtime::engine::EditingAuthority {
                    read: &grant,
                    create: create.as_ref(),
                    edit: edit.as_ref().ok_or("separate edit consent required")?,
                },
            )?
        }
        "start" if config.skill.is_some() => journal.start_agent_skill_reviewed(
            &config.run,
            config.skill.as_ref().expect("selected skill"),
            config.skill_activation_revision,
            config.prompt,
            &grant,
        )?,
        "start" => {
            let messages = vec![Message::plain(Role::User, config.prompt)];
            #[cfg(target_vendor = "apple")]
            {
                if let Some(edit) = &edit {
                    journal.start_agent_editing(
                        &config.run,
                        messages,
                        orange_agent_runtime::engine::EditingAuthority {
                            read: &grant,
                            create: create.as_ref(),
                            edit,
                        },
                        config.user_questions,
                    )?
                } else {
                    start_base(
                        &mut journal,
                        &config.run,
                        messages,
                        &grant,
                        create.as_ref(),
                        config.user_questions,
                    )?
                }
            }
            #[cfg(not(target_vendor = "apple"))]
            start_base(
                &mut journal,
                &config.run,
                messages,
                &grant,
                create.as_ref(),
                config.user_questions,
            )?
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
        "approve-create" => {
            if args.len() != 5 {
                return Err("operation ID and reviewed argument digest required".into());
            }
            let create = create.as_ref().ok_or("explicit consent ID required")?;
            let seconds = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            )?;
            journal
                .approve_agent_creation(
                    &config.run.owner,
                    &config.run.run_id,
                    CreationApproval {
                        operation_id: &args[3],
                        argument_digest: &args[4],
                        expires_at: (seconds + 60).min(config.run.deadline_unix),
                    },
                    &grant,
                    create,
                )
                .await?;
            journal
                .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
                .await?
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
            if journal
                .run_skill(&config.run.owner, &config.run.run_id)?
                .is_some_and(|p| {
                    matches!(
                        p.id.as_str(),
                        "orange.code-review" | "orange.reviewed-patch"
                    )
                })
            {
                verified_model::check(
                    &config.model_key_file,
                    config.model_socket.as_deref(),
                    &config.run.model_version,
                    config.context_tokens,
                )?;
            }
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
            let model = LocalModel(client.with_protocol(config.model_protocol));
            let (sender, receiver) = watch::channel(false);
            let signal = tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = sender.send(true);
            });
            #[cfg(target_vendor = "apple")]
            let result = if let Some(edit) = &edit {
                journal
                    .advance_agent_editing(
                        &config.run.owner,
                        &config.run.run_id,
                        orange_agent_runtime::engine::EditingAuthority {
                            read: &grant,
                            create: create.as_ref(),
                            edit,
                        },
                        &model,
                        receiver,
                    )
                    .await
            } else {
                advance_base(
                    &mut journal,
                    &config.run,
                    &grant,
                    create.as_ref(),
                    &model,
                    receiver,
                )
                .await
            };
            #[cfg(not(target_vendor = "apple"))]
            let result = advance_base(
                &mut journal,
                &config.run,
                &grant,
                create.as_ref(),
                &model,
                receiver,
            )
            .await;
            signal.abort();
            #[cfg(target_vendor = "apple")]
            if let Some(edit) = &edit {
                edit.wait_drained().await;
            }
            let view = result?;
            grant.wait_drained().await;
            if let Some(create) = &create {
                create.wait_drained().await;
            }
            view
        }
        _ => return Err("unknown command".into()),
    };
    let view = if journal
        .run_skill(&config.run.owner, &config.run.run_id)?
        .is_some()
        || view.final_text.is_some()
        || !view.pending.is_empty()
        || view.user_question.is_some()
    {
        journal
            .agent_view_authorized(&config.run.owner, &config.run.run_id, &grant)
            .await?
    } else {
        view
    };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

fn start_base(
    journal: &mut Journal,
    run: &RunConfig,
    messages: Vec<Message>,
    read: &WorkspaceGrant,
    create: Option<&orange_agent_runtime::workspace::creation::CreateFileGrant>,
    questions: bool,
) -> Result<orange_agent_runtime::engine::RunView, orange_agent_runtime::engine::EngineError> {
    if let Some(create) = create {
        journal.start_agent_creating(run, messages, read, create, questions)
    } else if questions {
        journal.start_agent_interactive(run, messages, read)
    } else {
        journal.start_agent(run, messages, read)
    }
}
async fn advance_base(
    journal: &mut Journal,
    run: &RunConfig,
    read: &WorkspaceGrant,
    create: Option<&orange_agent_runtime::workspace::creation::CreateFileGrant>,
    model: &impl ModelGateway,
    cancel: watch::Receiver<bool>,
) -> Result<orange_agent_runtime::engine::RunView, orange_agent_runtime::engine::EngineError> {
    if let Some(create) = create {
        journal
            .advance_agent_creating(&run.owner, &run.run_id, read, create, model, cancel)
            .await
    } else {
        journal
            .advance_agent(&run.owner, &run.run_id, read, model, cancel)
            .await
    }
}
