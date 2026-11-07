use super::*;
use crate::storage::private_temp;
use orange_protocol::model::ToolCall;
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

/// Test-only scripted transport. No real inference or model quality claim.
struct ScriptedModel {
    responses: Mutex<std::collections::VecDeque<ResponseCandidate>>,
    calls: AtomicUsize,
    requests: Mutex<Vec<GenerateRequest>>,
}
impl ScriptedModel {
    fn new(responses: Vec<ResponseCandidate>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
        }
    }
}
impl ModelGateway for ScriptedModel {
    fn generate<'a>(
        &'a self,
        request: &'a GenerateRequest,
        _: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            request.validate().expect("validated request");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().expect("lock").push(request.clone());
            self.responses
                .lock()
                .expect("lock")
                .pop_front()
                .ok_or(GatewayFailure)
        })
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}
fn tools(calls: Vec<ToolCall>) -> ResponseCandidate {
    ResponseCandidate {
        response_id: "response-1".into(),
        text: String::new(),
        tool_calls: calls,
        native_usage: None,
        finish_reason: "tool_calls".into(),
    }
}
fn final_response() -> ResponseCandidate {
    ResponseCandidate {
        response_id: "response-final".into(),
        text: "Read the authorized content.".into(),
        tool_calls: vec![],
        native_usage: None,
        finish_reason: "stop".into(),
    }
}
struct Fixture {
    private: tempfile::TempDir,
    workspace: tempfile::TempDir,
    grant: WorkspaceGrant,
    config: RunConfig,
}
impl Fixture {
    fn new() -> Self {
        let private = private_temp();
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("note.txt"), "allowed data").expect("file");
        let grant = WorkspaceGrant::open(
            "alice",
            "grant-v1",
            "policy-v1",
            workspace.path(),
            &[private.path().to_owned()],
        )
        .expect("grant");
        let config = RunConfig {
            owner: "alice".into(),
            run_id: "run-1".into(),
            policy: "policy-v1".into(),
            environment: grant.environment().into(),
            model_version: "local-test-profile".into(),
            max_steps: 20,
            max_invocations: 10,
            max_output_tokens: 512,
            deadline_unix: now().expect("time") + 600,
        };
        Self {
            private,
            workspace,
            grant,
            config,
        }
    }
    fn open(&self) -> Journal {
        Journal::open(&self.private.path().join("journal.sqlite")).expect("journal")
    }
    fn start(&self, j: &mut Journal) {
        j.start_agent(
            &self.config,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            &self.grant,
        )
        .expect("start");
    }
    async fn advance(
        &self,
        j: &mut Journal,
        model: &impl ModelGateway,
        rx: watch::Receiver<bool>,
    ) -> Result<RunView, EngineError> {
        j.advance_agent("alice", "run-1", &self.grant, model, rx)
            .await
    }
    fn approve(&self, j: &Journal) {
        let pending = j.agent_view("alice", "run-1").expect("view").pending;
        for op in pending {
            j.approve_agent_tool(
                "alice",
                "run-1",
                &op.operation_id,
                now().expect("time") + 60,
                &self.grant,
            )
            .expect("approval");
        }
    }
}
#[tokio::test]
async fn response_intent_approval_tool_result_and_reopen_complete_once() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "call-1",
            "read_file",
            json!({"path":"note.txt"}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let view = f.advance(&mut j, &model, rx.clone()).await.expect("model");
    assert_eq!(view.phase, Phase::ToolsPending);
    assert_eq!(view.pending.len(), 1);
    let op_id = view.pending[0].operation_id.clone();
    assert!(matches!(
        f.advance(&mut j, &model, rx.clone()).await,
        Err(EngineError::ApprovalRequired)
    ));
    assert_eq!(
        j.state("alice", &op_id).expect("state").as_deref(),
        Some("intended")
    );
    assert!(
        j.approve_agent_tool("bob", "run-1", &op_id, now().expect("time") + 20, &f.grant)
            .is_err()
    );
    assert!(
        j.approve_agent_tool("alice", "run-1", &op_id, now().expect("time") - 1, &f.grant)
            .is_err()
    );
    f.approve(&j);
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("tool")
            .phase,
        Phase::Ready
    );
    assert_eq!(
        j.state("alice", &op_id).expect("state").as_deref(),
        Some("succeeded")
    );
    drop(j);
    let mut j = f.open();
    let view = f.advance(&mut j, &model, rx.clone()).await.expect("final");
    assert_eq!(view.phase, Phase::Completed);
    assert_eq!(view.steps, 3);
    assert_eq!(view.invocations, 2);
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("idempotent terminal")
            .phase,
        Phase::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let requests = model.requests.lock().expect("requests");
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        1
    );
    assert!(
        requests[1]
            .messages
            .last()
            .expect("result")
            .content
            .contains("allowed data")
    );
    assert!(j.agent_view("bob", "run-1").is_err());
}
#[tokio::test]
async fn one_invalid_tool_rejects_whole_response_before_any_intent() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![tools(vec![
        call("ok", "read_file", json!({"path":"note.txt"})),
        call("unsafe", "shell", json!({"command":"touch escaped"})),
    ])]);
    let (_tx, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    let operations: i64 = j
        .connection
        .query_row("SELECT count(*) FROM operations", [], |r| r.get(0))
        .expect("count");
    let responses: i64 = j
        .connection
        .query_row("SELECT count(*) FROM responses", [], |r| r.get(0))
        .expect("count");
    assert_eq!((operations, responses), (0, 0));
    assert_eq!(
        j.agent_view("alice", "run-1").expect("view").phase,
        Phase::RecoveryRequired
    );
}
#[tokio::test]
async fn deleted_or_modified_tool_source_is_not_sent_to_next_model() {
    for delete in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        f.start(&mut j);
        let model = ScriptedModel::new(vec![
            tools(vec![call(
                "call-1",
                "read_file",
                json!({"path":"note.txt"}),
            )]),
            final_response(),
        ]);
        let (_tx, rx) = watch::channel(false);
        f.advance(&mut j, &model, rx.clone()).await.expect("model");
        f.approve(&j);
        f.advance(&mut j, &model, rx.clone()).await.expect("read");
        drop(j);
        if delete {
            std::fs::remove_file(f.workspace.path().join("note.txt")).expect("delete");
        } else {
            std::fs::write(f.workspace.path().join("note.txt"), "replacement").expect("change");
        }
        let mut j = f.open();
        assert_eq!(
            f.advance(&mut j, &model, rx).await.expect("pause").phase,
            Phase::RecoveryRequired
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn failure_is_explicit_and_multiple_calls_keep_complete_history() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![
            call("missing", "read_file", json!({"path":"missing.txt"})),
            call("present", "read_file", json!({"path":"note.txt"})),
        ]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let first = f.advance(&mut j, &model, rx.clone()).await.expect("model");
    f.approve(&j);
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("failure recorded")
            .phase,
        Phase::ToolsPending
    );
    assert_eq!(
        j.state("alice", &first.pending[0].operation_id)
            .expect("failed")
            .as_deref(),
        Some("failed")
    );
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("read")
            .phase,
        Phase::Ready
    );
    assert_eq!(
        f.advance(&mut j, &model, rx).await.expect("final").phase,
        Phase::Completed
    );
    let requests = model.requests.lock().expect("requests");
    let messages = &requests[1].messages;
    assert_eq!(messages.iter().filter(|m| m.role == Role::Tool).count(), 2);
    assert!(
        messages
            .iter()
            .any(|m| m.content.contains("filesystem_error"))
    );
}
struct NeverModel {
    called: AtomicUsize,
}
impl ModelGateway for NeverModel {
    fn generate<'a>(&'a self, _: &'a GenerateRequest, _: watch::Receiver<bool>) -> ModelFuture<'a> {
        Box::pin(async move {
            self.called.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn dropped_model_future_and_reopen_never_replays_or_bypasses_quarantine() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = NeverModel {
        called: AtomicUsize::new(0),
    };
    let (_tx, rx) = watch::channel(false);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            f.advance(&mut j, &model, rx.clone())
        )
        .await
        .is_err()
    );
    assert_eq!(model.called.load(Ordering::SeqCst), 1);
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx).await.expect("recovery").phase,
        Phase::RecoveryRequired
    );
    assert_eq!(model.called.load(Ordering::SeqCst), 1);
    let mut config = f.config.clone();
    config.run_id = "another-run".into();
    assert!(matches!(
        j.start_agent(
            &config,
            vec![Message::plain(Role::User, "new".into())],
            &f.grant
        ),
        Err(EngineError::RecoveryRequired)
    ));
}
#[tokio::test]
async fn cancellation_during_model_and_revocation_before_tool_are_durable() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = NeverModel {
        called: AtomicUsize::new(0),
    };
    let (tx, rx) = watch::channel(false);
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(true).expect("cancel");
    };
    let (result, ()) = tokio::join!(f.advance(&mut j, &model, rx.clone()), cancel);
    assert_eq!(result.expect("cancelled").phase, Phase::Cancelled);
    assert!(f.grant.is_revoked());
    drop(j);
    let j = f.open();
    // Reopen conservatively exposes unknown model execution as recovery_required.
    assert_eq!(
        j.agent_view("alice", "run-1").expect("view").phase,
        Phase::RecoveryRequired
    );
    let active: bool = j
        .connection
        .query_row("SELECT active FROM runs", [], |r| r.get(0))
        .expect("active");
    assert!(!active);

    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![tools(vec![call(
        "call-1",
        "read_file",
        json!({"path":"note.txt"}),
    )])]);
    let (_tx, rx) = watch::channel(false);
    let view = f.advance(&mut j, &model, rx.clone()).await.expect("model");
    f.approve(&j);
    f.grant.request_revoke();
    assert_eq!(
        f.advance(&mut j, &model, rx).await.expect("cancel").phase,
        Phase::Cancelled
    );
    assert_eq!(
        j.state("alice", &view.pending[0].operation_id)
            .expect("state")
            .as_deref(),
        Some("intended")
    );
}
#[tokio::test]
async fn persisted_budget_does_not_reset_on_restart() {
    let mut f = Fixture::new();
    f.config.max_steps = 1;
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![tools(vec![call(
        "call-1",
        "read_file",
        json!({"path":"note.txt"}),
    )])]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone()).await.expect("model");
    f.approve(&j);
    drop(j);
    let mut j = f.open();
    let view = f.advance(&mut j, &model, rx).await.expect("limited");
    assert_eq!(view.phase, Phase::Limited);
    assert_eq!(view.steps, 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn committed_result_is_applied_once_without_dispatch_after_crash() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "call-1",
            "read_file",
            json!({"path":"note.txt"}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone()).await.expect("model");
    f.approve(&j);
    // Crash injection precisely after result commit and before context commit.
    let stored = j.stored_run("alice", "run-1").expect("run");
    let op = j.pending_operations("run-1").expect("ops").remove(0);
    j.claim(&op.operation(&stored.config), now().expect("time"))
        .expect("claim");
    let receipt = f
        .grant
        .execute(
            "alice",
            f.grant.environment(),
            "policy-v1",
            op.request.clone(),
        )
        .await
        .expect("read");
    j.connection.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=? WHERE operation_id=?",params![digest("tool-result-v1",receipt.result()).expect("digest"),encode(receipt.result()).expect("json"),receipt.model_text(),op.id]).expect("persist");
    drop(receipt);
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("apply")
            .phase,
        Phase::Ready
    );
    assert_eq!(
        f.advance(&mut j, &model, rx).await.expect("final").phase,
        Phase::Completed
    );
    let dispatched: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.dispatching'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(dispatched, 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn revoked_completed_run_cannot_reexpose_final_text_and_config_cannot_reset() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let mut changed = f.config.clone();
    changed.max_steps += 1;
    assert!(matches!(
        j.start_agent(
            &changed,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            &f.grant
        ),
        Err(EngineError::Journal(JournalError::Conflict))
    ));
    let model = ScriptedModel::new(vec![final_response()]);
    let (_tx, rx) = watch::channel(false);
    assert!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("final")
            .final_text
            .is_some()
    );
    j.revoke_run("alice", "run-1").expect("revoke");
    drop(j);
    let j = f.open();
    assert!(
        j.agent_view("alice", "run-1")
            .expect("view")
            .final_text
            .is_none()
    );
}

#[tokio::test]
async fn authorized_recovery_view_refuses_deleted_or_changed_completed_sources() {
    for delete in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        f.start(&mut j);
        let model = ScriptedModel::new(vec![
            tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
            final_response(),
        ]);
        let (_sender, rx) = watch::channel(false);
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("proposal");
        f.approve(&j);
        f.advance(&mut j, &model, rx.clone()).await.expect("read");
        f.advance(&mut j, &model, rx).await.expect("complete");
        assert!(
            j.agent_view_authorized("alice", "run-1", &f.grant)
                .await
                .expect("current sources")
                .final_text
                .is_some()
        );
        assert!(j.agent_config("other", "run-1").is_err());
        assert!(j.agent_workspace_id("other", "run-1").is_err());
        assert_eq!(
            j.agent_config("alice", "run-1")
                .expect("metadata")
                .model_version,
            f.config.model_version
        );
        drop(j);
        if delete {
            std::fs::remove_file(f.workspace.path().join("note.txt")).expect("delete");
        } else {
            std::fs::write(f.workspace.path().join("note.txt"), "changed").expect("replace");
        }
        let j = f.open();
        assert!(
            j.agent_view_authorized("alice", "run-1", &f.grant)
                .await
                .is_err(),
            "reopening must not restore unavailable source text"
        );
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "view validation never reruns model"
        );
        drop(j);
        let mut j = f.open();
        assert!(j.cancel_agent_run("another-owner", "run-1").is_err());
        let cancelled = j
            .cancel_agent_run("alice", "run-1")
            .expect("owner can stop without reopening deleted source");
        // Revocation hides content; it must not rewrite a completed history.
        assert_eq!(cancelled.phase, Phase::Completed);
        assert!(cancelled.final_text.is_none());
    }
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    f.grant.request_revoke();
    std::fs::remove_file(f.workspace.path().join("note.txt")).expect("deleted source");
    drop(j);
    let mut j = f.open();
    assert_eq!(
        j.cancel_agent_run("alice", "run-1")
            .expect("stop active run without filesystem authority")
            .phase,
        Phase::Cancelled
    );
}

async fn search_approval_recovery(tool: &str, args: serde_json::Value, version: &str) {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![call("search-call", tool, args)]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("model candidate committed");
    assert_eq!(pending.phase, Phase::ToolsPending);
    assert_eq!(pending.pending.len(), 1);
    let operation = pending.pending[0].operation_id.clone();
    assert_eq!(pending.pending[0].tool_version, version);
    assert!(matches!(
        f.advance(&mut j, &model, rx.clone()).await,
        Err(EngineError::ApprovalRequired)
    ));
    assert_eq!(
        j.state("alice", &operation).expect("state").as_deref(),
        Some("intended")
    );
    f.approve(&j);
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("authorized search")
            .phase,
        Phase::Ready
    );
    assert_eq!(
        j.state("alice", &operation).expect("state").as_deref(),
        Some("succeeded")
    );
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("continue")
            .phase,
        Phase::Completed
    );
    f.advance(&mut j, &model, rx)
        .await
        .expect("terminal idempotence");
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let requests = model.requests.lock().expect("requests");
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(results.len(), 1);
    assert!(
        serde_json::to_string(results[0])
            .expect("result")
            .contains("note.txt")
    );
}

#[tokio::test]
async fn file_search_requires_approval_and_persists_one_result_across_reopen() {
    search_approval_recovery(
        "search_files",
        json!({"path":".","glob":"**/*.txt"}),
        "search_files.v1",
    )
    .await;
}
#[tokio::test]
async fn text_search_requires_approval_and_persists_one_result_across_reopen() {
    search_approval_recovery(
        "search_text",
        json!({"path":".","query":"allowed","glob":"*.txt"}),
        "search_text.v1",
    )
    .await;
}
