//! Same owned harness is compiled against fixed schema27 and current Runtime.
use orange_agent_runtime::{
    Journal,
    engine::{
        AgentAuthority, AgentMemoryApproval, GatewayFailure, MemoryManagementAuthority,
        MemoryReadSelection, ModelFuture, ModelGateway, Phase, RunConfig,
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
struct Model(Mutex<VecDeque<ResponseCandidate>>);
impl ModelGateway for Model {
    fn generate<'a>(&'a self, _: &'a GenerateRequest, _: watch::Receiver<bool>) -> ModelFuture<'a> {
        Box::pin(async move {
            self.0
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
fn call(id: &str, name: &str, args: serde_json::Value) -> ResponseCandidate {
    ResponseCandidate {
        response_id: id.into(),
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }],
        native_usage: None,
        finish_reason: "tool_calls".into(),
    }
}
fn select(id: &str) -> MemoryReadSelection {
    MemoryReadSelection {
        entry_id: id.into(),
        expected_version: Some("2".into()),
        start_byte: 0,
        max_bytes: 4096,
        include_disabled: false,
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    assert_eq!(args.len(), 2);
    let root = PathBuf::from(&args[1]);
    let mut j = Journal::open(&root.join("private/journal.sqlite"))?;
    if args[0] == "inspect" {
        return Ok(());
    }
    if args[0] == "seed" {
        std::fs::write(root.join("workspace/note.txt"), "owned derived source")?;
        let ws = j.authorize_workspace(
            "alice",
            "owned-source",
            &root.join("workspace"),
            "policy-v1",
        )?;
        let read = j.open_workspace("alice", &ws.workspace_id)?;
        j.create_memory_scope("alice", "personal", false)?;
        let memories = vec![j.authorize_memory("alice", "personal", "policy-v1", true)?];
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
        let model=Model(Mutex::new(vec![call("read-file","read_file",json!({"path":"note.txt"})),call("learn","memory_write",json!({"scope_id":"personal","mutation":"put","payload":{"title":"Learned","content":"owned derived source","enabled":true}})),ResponseCandidate{response_id:"final".into(),text:"learned".into(),tool_calls:vec![],native_usage:None,finish_reason:"stop".into()}].into()));
        let (_tx, rx) = watch::channel(false);
        j.start_agent_authorized(
            &config,
            vec![Message::plain(
                Role::User,
                "Read the file and propose a memory.".into(),
            )],
            auth,
            false,
        )?;
        let view = j
            .advance_agent_authorized("alice", "run", auth, &model, rx.clone())
            .await?;
        j.approve_agent_tool(
            "alice",
            "run",
            &view.pending[0].operation_id,
            time() + 600,
            &read,
        )?;
        j.advance_agent_authorized("alice", "run", auth, &model, rx.clone())
            .await?;
        let view = j
            .advance_agent_authorized("alice", "run", auth, &model, rx.clone())
            .await?;
        let op = &view.pending[0].operation_id;
        let p = j.agent_memory_preview("alice", "run", op, auth).await?;
        j.approve_agent_memory(
            "alice",
            "run",
            AgentMemoryApproval {
                operation_id: op,
                preview_digest: &p.approval_digest,
                expires_at: time() + 600,
            },
            auth,
        )
        .await?;
        j.advance_agent_authorized("alice", "run", auth, &model, rx.clone())
            .await?;
        assert_eq!(
            j.advance_agent_authorized("alice", "run", auth, &model, rx)
                .await?
                .phase,
            Phase::Completed
        );
        let id = p.review.entry_id;
        j.register_run("alice", "human-confirm", "policy-v1")?;
        let sources = vec![read.clone()];
        let management =
            MemoryManagementAuthority::new(&memories[0]).with_source_workspaces(&sources);
        let edit = j
            .prepare_user_memory_write(
                management,
                "confirm",
                MemoryWriteRequest {
                    entry_id: id.clone(),
                    expected_version: Some("1".into()),
                    source_run: "human-confirm".into(),
                    origin: MemoryOrigin::UserDeclaration,
                    mutation: MemoryMutation::Put,
                    payload: Some(MemoryPayload {
                        title: "Confirmed".into(),
                        content: "owned derived source, confirmed".into(),
                        enabled: true,
                    }),
                },
            )
            .await?;
        j.approve_user_memory_write(management, &edit.review.draft_id, &edit.approval_digest)
            .await?;
        // Document the old executable's actual bug, not a simulated migration.
        assert!(
            j.read_memory_verified(MemoryManagementAuthority::new(&memories[0]), select(&id))
                .await
                .is_ok()
        );
        std::fs::write(root.join("entry-id"), id)?;
    } else {
        assert_eq!(args[0], "resume");
        let id = std::fs::read_to_string(root.join("entry-id"))?;
        let scope = j.authorize_memory("alice", "personal", "policy-v1", true)?;
        assert!(
            j.read_memory_verified(MemoryManagementAuthority::new(&scope), select(&id))
                .await
                .is_err()
        );
        assert!(
            j.read_memory(&scope, &id, Some("2"), 0, 4096, false)
                .is_err()
        );
        let workspace = j.agent_workspace_id("alice", "run")?.expect("saved root");
        let read = j.open_workspace("alice", &workspace)?;
        let sources = vec![read];
        let management = MemoryManagementAuthority::new(&scope).with_source_workspaces(&sources);
        let receipt = j.read_memory_verified(management, select(&id)).await?;
        assert_eq!(receipt.view(&j)?.content, "owned derived source, confirmed");
        drop(receipt);
        let current = j.workspace("alice", &workspace)?;
        j.revoke_workspace("alice", &workspace, &current.version)?;
        assert!(
            j.read_memory_verified(management, select(&id))
                .await
                .is_err()
        );
        assert!(j.read_memory(&scope, &id, None, 0, 4096, false).is_err());
    }
    Ok(())
}
