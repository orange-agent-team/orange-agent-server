//! Compiled unchanged against each pinned Runtime; deterministic transport only.
use orange_agent_runtime::{
    Journal,
    engine::{
        AgentAuthority, AgentMemoryApproval, GatewayFailure, ModelFuture, ModelGateway, Phase,
        RunConfig,
    },
    memory::{MemoryMutation, MemoryOrigin, MemoryPayload, MemoryWriteRequest},
};
use orange_protocol::model::{GenerateRequest, Message, ResponseCandidate, Role, ToolCall};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
struct Model {
    replies: Mutex<VecDeque<ResponseCandidate>>,
    finishing: bool,
}
impl ModelGateway for Model {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        _: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            let read = request
                .tools
                .iter()
                .find(|t| t.name == "memory_read")
                .expect("legacy read");
            assert!(
                read.parameters["properties"].get("mode").is_none(),
                "old Run must retain v1 descriptor"
            );
            if self.finishing {
                assert!(
                    request.messages.iter().any(
                        |m| m.role == Role::Tool && m.content.contains("Owned original memory")
                    )
                );
            }
            self.replies
                .lock()
                .expect("queue")
                .pop_front()
                .ok_or(GatewayFailure)
        })
    }
}
fn time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    assert_eq!(args.len(), 2);
    let root = PathBuf::from(&args[1]);
    let mut journal = Journal::open(&root.join("private/journal.sqlite"))?;
    if args[0] == "inspect" {
        return Ok(());
    }
    if args[0] == "seed" {
        let workspace = journal.authorize_workspace(
            "alice",
            "owned-workspace",
            &root.join("workspace"),
            "policy-v1",
        )?;
        let read = journal.open_workspace("alice", &workspace.workspace_id)?;
        journal.create_memory_scope("alice", "personal", false)?;
        journal.register_run("alice", "user-seed", "policy-v1")?;
        let memory = journal.authorize_memory("alice", "personal", "policy-v1", true)?;
        let draft = journal.prepare_memory_write(
            &memory,
            "seed-memory",
            MemoryWriteRequest {
                entry_id: "entry".into(),
                expected_version: None,
                source_run: "user-seed".into(),
                origin: MemoryOrigin::UserDeclaration,
                mutation: MemoryMutation::Put,
                payload: Some(MemoryPayload {
                    title: "Owned note".into(),
                    content: "Owned original memory".into(),
                    enabled: true,
                }),
            },
        )?;
        let hash = draft.view(&journal)?.digest.clone();
        journal.approve_memory_write(draft, &hash)?;
        let memories = vec![memory];
        let auth = AgentAuthority::read(&read).with_memory(&memories);
        let config = RunConfig {
            owner: "alice".into(),
            run_id: "run".into(),
            policy: "policy-v1".into(),
            environment: read.environment().into(),
            model_version: "scripted-compat".into(),
            max_steps: 20,
            max_invocations: 10,
            max_output_tokens: 512,
            deadline_unix: time() + 3600,
        };
        journal.start_agent_authorized(
            &config,
            vec![Message::plain(Role::User, "Read my approved note.".into())],
            auth,
            false,
        )?;
        let model=Model{finishing:false,replies:Mutex::new(vec![ResponseCandidate{response_id:"first".into(),text:String::new(),tool_calls:vec![ToolCall{id:"read".into(),name:"memory_read".into(),arguments:json!({"scope_id":"personal","entry_id":"entry","expected_version":"1"})}],native_usage:None,finish_reason:"tool_calls".into()}].into())};
        let (_tx, rx) = watch::channel(false);
        let view = journal
            .advance_agent_authorized("alice", "run", auth, &model, rx.clone())
            .await?;
        let op = &view.pending[0];
        journal
            .approve_agent_memory(
                "alice",
                "run",
                AgentMemoryApproval {
                    operation_id: &op.operation_id,
                    preview_digest: &op.argument_digest,
                    expires_at: time() + 600,
                },
                auth,
            )
            .await?;
        assert_eq!(
            journal
                .advance_agent_authorized("alice", "run", auth, &model, rx)
                .await?
                .phase,
            Phase::Ready
        );
    } else {
        assert_eq!(args[0], "resume");
        let id = journal
            .agent_workspace_id("alice", "run")?
            .expect("registered root");
        let read = journal.open_workspace("alice", &id)?;
        let memories = vec![journal.authorize_memory("alice", "personal", "policy-v1", true)?];
        let auth = AgentAuthority::read(&read).with_memory(&memories);
        let model = Model {
            finishing: true,
            replies: Mutex::new(
                vec![ResponseCandidate {
                    response_id: "last".into(),
                    text: "memory finished".into(),
                    tool_calls: vec![],
                    native_usage: None,
                    finish_reason: "stop".into(),
                }]
                .into(),
            ),
        };
        let (_tx, rx) = watch::channel(false);
        let view = journal
            .advance_agent_authorized("alice", "run", auth, &model, rx)
            .await?;
        assert_eq!(view.phase, Phase::Completed);
        assert_eq!(view.final_text.as_deref(), Some("memory finished"));
    }
    Ok(())
}
