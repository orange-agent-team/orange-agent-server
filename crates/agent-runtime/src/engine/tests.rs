mod contracts;
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

fn question_response() -> ResponseCandidate {
    tools(vec![call(
        "question-call",
        "ask_user",
        json!({"questions":[{"prompt":"Which file should be used?","choices":["note.txt","another.txt"]}]}),
    )])
}
fn start_interactive(f: &Fixture, j: &mut Journal) {
    j.start_agent_interactive(
        &f.config,
        vec![Message::plain(Role::User, "Clarify the task".into())],
        &f.grant,
    )
    .expect("interactive start");
}
fn user_answer(view: &RunView, text: &str) -> orange_protocol::questions::AnswerUser {
    let question = view.user_question.as_ref().expect("committed question");
    orange_protocol::questions::AnswerUser {
        question_id: question.question_id.clone(),
        argument_digest: question.argument_digest.clone(),
        answers: vec![text.into()],
    }
}
#[tokio::test]
async fn user_question_reopens_and_answers_once_without_approving_tools() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![
        question_response(),
        tools(vec![call(
            "read-after-answer",
            "read_file",
            json!({"path":"note.txt"}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("question");
    assert_eq!(pending.phase, Phase::AwaitingUser);
    assert!(pending.pending.is_empty());
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("waiting does not dispatch")
            .phase,
        Phase::AwaitingUser
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let answer = user_answer(&pending, "Yes, approve everything");
    assert!(matches!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &answer.question_id,
            now().expect("time") + 30,
            &f.grant
        ),
        Err(EngineError::ApprovalRequired)
    ));
    drop(j);
    let mut j = f.open();
    assert_eq!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .expect("recovered current question")
            .user_question
            .expect("question")
            .question_id,
        answer.question_id
    );
    let ready = j
        .answer_agent_question("alice", "run-1", &answer, &f.grant, rx.clone())
        .await
        .expect("answer");
    assert_eq!(ready.phase, Phase::Ready);
    assert!(ready.user_question.is_none());
    assert_eq!(ready.steps, 2);
    assert_eq!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx.clone())
            .await
            .expect("same reply")
            .steps,
        2
    );
    let mut changed = answer.clone();
    changed.answers = vec!["different".into()];
    assert!(matches!(
        j.answer_agent_question("alice", "run-1", &changed, &f.grant, rx.clone())
            .await,
        Err(EngineError::Journal(JournalError::Conflict))
    ));
    let next = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("next model");
    assert_eq!(next.phase, Phase::ToolsPending);
    assert_eq!(next.pending.len(), 1);
    assert!(matches!(
        f.advance(&mut j, &model, rx.clone()).await,
        Err(EngineError::ApprovalRequired)
    ));
    let approvals: i64 = j
        .connection
        .query_row("SELECT count(*) FROM approvals", [], |r| r.get(0))
        .expect("no implicit approval");
    assert_eq!(approvals, 0);
    let request = model.requests.lock().expect("requests")[1].clone();
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("question-call"))
            .count(),
        1
    );
    f.approve(&j);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("explicitly approved read");
    assert_eq!(
        f.advance(&mut j, &model, rx).await.expect("final").phase,
        Phase::Completed
    );
}
#[tokio::test]
async fn user_question_capability_is_opt_in_and_mixed_responses_commit_nothing() {
    for interactive in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        let response = if interactive {
            start_interactive(&f, &mut j);
            let mut r = question_response();
            r.tool_calls
                .push(call("mixed-read", "read_file", json!({"path":"note.txt"})));
            r
        } else {
            f.start(&mut j);
            question_response()
        };
        let model = ScriptedModel::new(vec![response]);
        let (_tx, rx) = watch::channel(false);
        assert!(f.advance(&mut j, &model, rx).await.is_err());
        for table in ["responses", "runtime_questions", "operations"] {
            let count: i64 = j
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .expect("no partial commit");
            assert_eq!(count, 0);
        }
        let offered = model.requests.lock().expect("request")[0]
            .tools
            .iter()
            .any(|t| t.name == "ask_user");
        assert_eq!(offered, interactive);
    }
}
#[tokio::test]
async fn user_question_answer_checks_owner_digest_cancel_and_revocation() {
    for mode in ["owner", "digest", "cancel", "revoke"] {
        let f = Fixture::new();
        let mut j = f.open();
        start_interactive(&f, &mut j);
        let model = ScriptedModel::new(vec![question_response()]);
        let (_tx, rx) = watch::channel(false);
        let pending = f
            .advance(&mut j, &model, rx.clone())
            .await
            .expect("question");
        let mut answer = user_answer(&pending, "note.txt");
        if mode == "digest" {
            answer.argument_digest = "f".repeat(64);
        }
        if mode == "cancel" {
            j.cancel_agent_run("alice", "run-1").expect("cancel");
        }
        if mode == "revoke" {
            f.grant.request_revoke();
        }
        let result = j
            .answer_agent_question(
                if mode == "owner" { "bob" } else { "alice" },
                "run-1",
                &answer,
                &f.grant,
                rx,
            )
            .await;
        assert!(result.is_err(), "{mode}");
        let accepted: i64 = j
            .connection
            .query_row(
                "SELECT count(*) FROM runtime_questions WHERE answers IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .expect("no answer");
        assert_eq!(accepted, 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn user_question_answer_transaction_rolls_back_and_budget_closes_pending_question() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![question_response()]);
    let (_tx, rx) = watch::channel(false);
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("question");
    let answer = user_answer(&pending, "note.txt");
    j.connection.execute_batch("CREATE TRIGGER answer_fault BEFORE INSERT ON events WHEN NEW.kind='question.answered' BEGIN SELECT RAISE(ABORT,'explicit answer commit fault'); END;").expect("fault");
    assert!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx.clone())
            .await
            .is_err()
    );
    j.connection
        .execute_batch("DROP TRIGGER answer_fault;")
        .expect("remove exact fault");
    let pending = j.agent_view("alice", "run-1").expect("rolled back");
    assert_eq!(pending.phase, Phase::AwaitingUser);
    assert_eq!(pending.steps, 1);
    assert!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await
            .is_ok()
    );
    let mut f = Fixture::new();
    f.config.max_steps = 1;
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![question_response()]);
    let (_tx, rx) = watch::channel(false);
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("last allowed model step");
    let limited = j
        .answer_agent_question(
            "alice",
            "run-1",
            &user_answer(&pending, "note.txt"),
            &f.grant,
            rx,
        )
        .await
        .expect("limit");
    assert_eq!(limited.phase, Phase::Limited);
    assert!(limited.user_question.is_none());
    let state: String = j
        .connection
        .query_row("SELECT state FROM runtime_questions", [], |r| r.get(0))
        .expect("closed question");
    assert_eq!(state, "cancelled");
}
#[tokio::test]
async fn user_question_changed_source_hides_question_and_blocks_answer() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "source-read",
            "read_file",
            json!({"path":"note.txt"}),
        )]),
        question_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("source intent");
    f.approve(&j);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("read source");
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("question after source");
    let answer = user_answer(&pending, "note.txt");
    std::fs::remove_file(f.workspace.path().join("note.txt")).expect("delete fixture source");
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
    assert!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await
            .is_err()
    );
    let count: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM runtime_questions WHERE state='answered'",
            [],
            |r| r.get(0),
        )
        .expect("no accepted reply");
    assert_eq!(count, 0);
    assert!(
        j.cancel_agent_run("alice", "run-1")
            .expect("stop without source")
            .user_question
            .is_none()
    );
}
#[test]
fn user_question_migration_preserves_sparse_run_cursor_and_denies_implicit_capability() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    j.connection.execute_batch("UPDATE runtime_runs SET rowid=37; DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; DROP TABLE runtime_tool_approvals; DROP TABLE runtime_tool_catalogs; DROP TABLE runtime_catalog_legacy_approvals; DROP TABLE runtime_catalog_legacy; DROP TABLE runtime_creation_capabilities; DROP TABLE creation_consent_audit; DROP TABLE creation_request_keys; DROP TABLE creation_consents; DROP TABLE runtime_questions; DROP TABLE runtime_capabilities; PRAGMA user_version=3;").expect("prior-version fixture with sparse cursor");
    drop(j);
    let mut j = f.open();
    let rowid: i64 = j
        .connection
        .query_row("SELECT rowid FROM runtime_runs", [], |r| r.get(0))
        .expect("cursor");
    assert_eq!(rowid, 37);
    assert!(
        !j.stored_run("alice", "run-1")
            .expect("legacy")
            .user_questions
    );
    let broken: i64 = j
        .connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .expect("foreign keys");
    assert_eq!(broken, 0);
    assert!(
        j.start_agent_interactive(
            &f.config,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            &f.grant
        )
        .is_err(),
        "existing Run cannot silently gain questions"
    );
}

#[tokio::test]
async fn user_question_prior_answer_cannot_answer_a_new_question() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let mut second = question_response();
    second.response_id = "response-two".into();
    second.tool_calls[0].id = "question-call-two".into();
    let model = ScriptedModel::new(vec![question_response(), second]);
    let (_tx, rx) = watch::channel(false);
    let first = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("first question");
    let answer = user_answer(&first, "note.txt");
    j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx.clone())
        .await
        .expect("first answer");
    assert!(
        j.connection
            .execute(
                "UPDATE runtime_questions SET answers='[]' WHERE question_id=?",
                [&answer.question_id]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "UPDATE runtime_capabilities SET user_questions=0 WHERE run_id='run-1'",
                []
            )
            .is_err()
    );
    let second = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("second question");
    let second_id = second
        .user_question
        .as_ref()
        .expect("second")
        .question_id
        .clone();
    assert_ne!(answer.question_id, second_id);
    let replay = j
        .answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
        .await
        .expect("read original answer outcome");
    assert_eq!(
        replay
            .user_question
            .expect("new question still pending")
            .question_id,
        second_id
    );
    let counts: (i64, i64) = j
        .connection
        .query_row(
            "SELECT sum(state='answered'),sum(state='pending') FROM runtime_questions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("decisions");
    assert_eq!(counts, (1, 1));
}
#[tokio::test]
async fn user_question_expired_or_cancelled_signal_never_accepts_answer() {
    for expire in [false, true] {
        let mut f = Fixture::new();
        if expire {
            f.config.deadline_unix = now().expect("clock") + 2;
        }
        let mut j = f.open();
        start_interactive(&f, &mut j);
        let model = ScriptedModel::new(vec![question_response()]);
        let (tx, rx) = watch::channel(false);
        let pending = f
            .advance(&mut j, &model, rx.clone())
            .await
            .expect("question");
        let answer = user_answer(&pending, "note.txt");
        if expire {
            while now().expect("clock") < f.config.deadline_unix {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } else {
            tx.send(true).expect("cancel signal");
        }
        let stopped = j
            .answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await
            .expect("bounded termination");
        assert_eq!(
            stopped.phase,
            if expire {
                Phase::Limited
            } else {
                Phase::Cancelled
            }
        );
        assert!(stopped.user_question.is_none());
        let accepted: i64 = j
            .connection
            .query_row(
                "SELECT count(*) FROM runtime_questions WHERE state='answered'",
                [],
                |r| r.get(0),
            )
            .expect("no reply");
        assert_eq!(accepted, 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn user_question_failed_request_event_rolls_back_response_and_question() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    j.connection.execute_batch("CREATE TRIGGER question_fault BEFORE INSERT ON events WHEN NEW.kind='question.requested' BEGIN SELECT RAISE(ABORT,'explicit question commit fault'); END;").expect("fault");
    let model = ScriptedModel::new(vec![question_response()]);
    let (_tx, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx.clone()).await.is_err());
    j.connection
        .execute_batch("DROP TRIGGER question_fault;")
        .expect("remove exact fault");
    for table in ["responses", "runtime_questions", "operations", "approvals"] {
        let count: i64 = j
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("atomic rollback");
        assert_eq!(count, 0);
    }
    assert_eq!(
        j.agent_view("alice", "run-1").expect("stopped").phase,
        Phase::RecoveryRequired
    );
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("no model replay")
            .phase,
        Phase::RecoveryRequired
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_question_foreign_response_reference_never_becomes_visible_or_answered() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![final_response()]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("first owner committed response");
    let foreign: String = j
        .connection
        .query_row("SELECT invocation FROM responses", [], |r| r.get(0))
        .expect("original response");
    let other_workspace = tempfile::tempdir().expect("other workspace");
    let grant = WorkspaceGrant::open(
        "bob",
        "bob-grant",
        "policy-v1",
        other_workspace.path(),
        &[f.private.path().to_owned()],
    )
    .expect("other explicit grant");
    let mut config = f.config.clone();
    config.owner = "bob".into();
    config.run_id = "bob-run".into();
    config.environment = grant.environment().into();
    j.start_agent_interactive(
        &config,
        vec![Message::plain(Role::User, "different task".into())],
        &grant,
    )
    .expect("other run");
    let body = orange_protocol::questions::AskUser::parse(
        &json!({"questions":[{"prompt":"forged cross-run provenance"}]}),
    )
    .expect("body");
    let digest = digest(
        "ask-user-intent-v1",
        &(
            &config.run_id,
            &config.owner,
            &config.environment,
            &config.policy,
            &body,
        ),
    )
    .expect("fixture digest");
    let forged_messages = vec![
        Message::plain(Role::User, "different task".into()),
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: vec![call(
                "forged-call",
                "ask_user",
                json!({"questions":[{"prompt":"forged cross-run provenance"}]}),
            )],
        },
    ];
    j.connection.execute("UPDATE runtime_runs SET phase='awaiting_user',active_invocation=?,messages=? WHERE run_id='bob-run'",params![foreign,encode(&forged_messages).expect("protocol-valid forged context")]).expect("explicit integrity fault");
    j.connection.execute("INSERT INTO runtime_questions(question_id,run_id,invocation,call_id,body,argument_digest,state) VALUES('forged-question','bob-run',?,'forged-call',?,?,'pending')",params![foreign,encode(&body).expect("body"),digest]).expect("explicit cross-run fixture");
    assert!(
        j.agent_view_authorized("bob", "bob-run", &grant)
            .await
            .is_err()
    );
    let answer = orange_protocol::questions::AnswerUser {
        question_id: "forged-question".into(),
        argument_digest: digest,
        answers: vec!["no authority".into()],
    };
    assert!(matches!(
        j.answer_agent_question("bob", "bob-run", &answer, &grant, rx)
            .await,
        Err(EngineError::RecoveryRequired)
    ));
    let accepted: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM runtime_questions WHERE state='answered'",
            [],
            |r| r.get(0),
        )
        .expect("no accepted answer");
    assert_eq!(accepted, 0);
}

#[tokio::test]
async fn user_question_requires_an_actual_ask_call_not_just_an_owned_response() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![final_response()]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("ordinary final response");
    let invocation: String = j
        .connection
        .query_row("SELECT invocation FROM responses", [], |r| r.get(0))
        .expect("owned response");
    let body = orange_protocol::questions::AskUser::parse(
        &json!({"questions":[{"prompt":"not actually requested by the committed model"}]}),
    )
    .expect("body");
    let argument_digest = digest(
        "ask-user-intent-v1",
        &(
            &f.config.run_id,
            &f.config.owner,
            &f.config.environment,
            &f.config.policy,
            &body,
        ),
    )
    .expect("digest");
    j.connection.execute("INSERT INTO runtime_questions(question_id,run_id,invocation,call_id,body,argument_digest,state) VALUES('unrequested-question','run-1',?,'unrequested-call',?,?,'pending')",params![invocation,encode(&body).expect("body"),argument_digest]).expect("explicit local integrity fault");
    j.connection
        .execute(
            "UPDATE runtime_runs SET phase='awaiting_user' WHERE run_id='run-1'",
            [],
        )
        .expect("fault phase");
    assert!(matches!(
        j.agent_view_authorized("alice", "run-1", &f.grant).await,
        Err(EngineError::RecoveryRequired)
    ));
    let answer = orange_protocol::questions::AnswerUser {
        question_id: "unrequested-question".into(),
        argument_digest,
        answers: vec!["not authority".into()],
    };
    assert!(matches!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await,
        Err(EngineError::RecoveryRequired)
    ));
    let replies: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM runtime_questions WHERE answers IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("no reply");
    assert_eq!(replies, 0);
}

#[tokio::test]
async fn user_question_waiting_run_keeps_private_root_guard_and_revoke_closes_it() {
    let f = Fixture::new();
    let mut j = f.open();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![question_response()]);
    let (_tx, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx)
        .await
        .expect("waiting question");
    let private_area = private_temp();
    assert!(matches!(
        j.reserve_private_directory(private_area.path()),
        Err(crate::workspaces::WorkspaceStoreError::RecoveryRequired)
    ));
    j.revoke_run("alice", "run-1")
        .expect("low-level owner revocation");
    let state: String = j
        .connection
        .query_row("SELECT state FROM runtime_questions", [], |r| r.get(0))
        .expect("closed question");
    assert_eq!(state, "cancelled");
}
#[tokio::test]
async fn user_question_workspace_revocation_durably_closes_pending_reply() {
    let mut f = Fixture::new();
    let mut j = f.open();
    let workspace = j
        .authorize_workspace(
            "alice",
            "registered-question-workspace",
            f.workspace.path(),
            "policy-v1",
        )
        .expect("actual persisted workspace");
    f.grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("fresh access");
    f.config.environment = f.grant.environment().into();
    start_interactive(&f, &mut j);
    let model = ScriptedModel::new(vec![question_response()]);
    let (_tx, rx) = watch::channel(false);
    let pending = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("question");
    let answer = user_answer(&pending, "note.txt");
    j.revoke_workspace("alice", &workspace.workspace_id, &workspace.version)
        .expect("workspace revocation");
    assert!(f.grant.is_revoked());
    assert!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await
            .is_err()
    );
    let state: String = j
        .connection
        .query_row("SELECT state FROM runtime_questions", [], |r| r.get(0))
        .expect("question closed atomically");
    assert_eq!(state, "cancelled");
    drop(j);
    let j = f.open();
    assert!(
        j.agent_view("alice", "run-1")
            .expect("metadata only")
            .user_question
            .is_none()
    );
}

#[tokio::test]
async fn unicode_path_policy_rejects_model_batch_before_response_or_approval() {
    let f = Fixture::new();
    std::fs::create_dir(f.workspace.path().join(".kube")).expect("owned protected fixture");
    std::fs::write(
        f.workspace.path().join(".kube/proof.txt"),
        "owned protected fixture",
    )
    .expect("fixture");
    let mut journal = f.open();
    f.start(&mut journal);
    let model = ScriptedModel::new(vec![tools(vec![
        call("ordinary", "read_file", json!({"path":"note.txt"})),
        call(
            "protected-alias",
            "read_file",
            json!({"path":".Kube/proof.txt"}),
        ),
    ])]);
    let (_cancel, rx) = watch::channel(false);
    assert!(f.advance(&mut journal, &model, rx.clone()).await.is_err());
    for table in ["responses", "operations", "approvals"] {
        let count = match table {
            "responses" => {
                journal
                    .connection
                    .query_row("SELECT count(*) FROM responses", [], |r| r.get::<_, i64>(0))
            }
            "operations" => {
                journal
                    .connection
                    .query_row("SELECT count(*) FROM operations", [], |r| {
                        r.get::<_, i64>(0)
                    })
            }
            _ => journal
                .connection
                .query_row("SELECT count(*) FROM approvals", [], |r| r.get::<_, i64>(0)),
        }
        .expect("actual durable count");
        assert_eq!(count, 0, "no partial response or effect: {table}");
    }
    drop(journal);
    let mut journal = f.open();
    assert_eq!(
        journal
            .agent_view("alice", "run-1")
            .expect("reopened")
            .phase,
        Phase::RecoveryRequired
    );
    assert_eq!(
        f.advance(&mut journal, &model, rx)
            .await
            .expect("recoverable view")
            .phase,
        Phase::RecoveryRequired
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "recovery must not redispatch the rejected response"
    );
}

fn creating(f: &Fixture, j: &mut Journal) -> CreateFileGrant {
    let grant = j
        .authorize_file_creation(
            "alice",
            "write-consent",
            &f.config.policy,
            f.workspace.path(),
        )
        .expect("separate trusted create consent");
    j.start_agent_creating(
        &f.config,
        vec![Message::plain(Role::User, "Create generated.txt".into())],
        &f.grant,
        &grant,
        false,
    )
    .expect("explicit capability start");
    grant
}
fn write_response() -> ResponseCandidate {
    tools(vec![call(
        "write-call",
        "write_file",
        json!({"path":"generated.txt","content":"actual created content"}),
    )])
}
async fn approve_creation(f: &Fixture, j: &Journal, grant: &CreateFileGrant) {
    let op = j
        .agent_view("alice", "run-1")
        .expect("preview")
        .pending
        .remove(0);
    j.approve_agent_creation(
        "alice",
        "run-1",
        CreationApproval {
            operation_id: &op.operation_id,
            argument_digest: &op.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        &f.grant,
        grant,
    )
    .await
    .expect("explicit current preview approval");
}
#[tokio::test]
async fn create_agent_requires_opt_in_exact_approval_and_revalidates_created_source() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let model = ScriptedModel::new(vec![write_response(), final_response()]);
    let (_stop, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx.clone()).await.is_err());
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "readonly entry cannot resume write-capable Run"
    );
    let view = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("complete proposal");
    assert_eq!(view.phase, Phase::ToolsPending);
    assert_eq!(view.pending[0].environment, create.environment());
    assert!(
        model.requests.lock().expect("requests")[0]
            .tools
            .iter()
            .any(|t| t.name == "write_file")
    );
    assert!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &view.pending[0].operation_id,
            now().expect("time") + 60,
            &f.grant
        )
        .is_err()
    );
    assert!(matches!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
            .await,
        Err(EngineError::ApprovalRequired)
    ));
    assert!(!f.workspace.path().join("generated.txt").exists());
    approve_creation(&f, &j, &create).await;
    let written = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("actual write");
    assert_eq!(written.phase, Phase::Ready);
    assert_eq!(written.steps, 2);
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("generated.txt")).expect("actual file"),
        "actual created content"
    );
    let done = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("final");
    assert_eq!(done.phase, Phase::Completed);
    assert!(
        model.requests.lock().expect("requests")[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.contains("write_file"))
    );
    std::fs::remove_file(f.workspace.path().join("generated.txt")).expect("source deletion");
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
    assert!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
            .await
            .is_err(),
        "terminal advance cannot re-expose deleted source context"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn create_agent_legacy_and_mismatched_capabilities_never_gain_write_intents() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let create = j
        .authorize_file_creation("alice", "new-cap", &f.config.policy, f.workspace.path())
        .expect("explicit separate consent");
    assert!(
        j.start_agent_creating(
            &f.config,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            &f.grant,
            &create,
            false
        )
        .is_err()
    );
    let model = ScriptedModel::new(vec![write_response()]);
    let (_stop, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    let effects: i64 = j
        .connection
        .query_row("SELECT count(*) FROM operations", [], |r| r.get(0))
        .expect("effects");
    assert_eq!(effects, 0);
    assert!(!f.workspace.path().join("generated.txt").exists());
}
#[tokio::test]
async fn create_agent_revoked_consent_blocks_pending_write_without_revoking_read_rights() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let model = ScriptedModel::new(vec![write_response()]);
    let (_stop, rx) = watch::channel(false);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("proposal");
    approve_creation(&f, &j, &create).await;
    j.revoke_file_creation("alice", create.consent_id(), &create.consent_version())
        .expect("revoke write rights");
    assert!(!f.grant.is_revoked());
    assert!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
            .await
            .is_err()
    );
    assert!(!f.workspace.path().join("generated.txt").exists());
    let dispatched: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.dispatching'",
            [],
            |r| r.get(0),
        )
        .expect("dispatches");
    assert_eq!(dispatched, 0);
}
#[tokio::test]
async fn create_agent_deleted_read_source_cannot_be_copied_after_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let model = ScriptedModel::new(vec![
        tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
        write_response(),
    ]);
    let (_stop, rx) = watch::channel(false);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("read intent");
    f.approve(&j);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("read");
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("write proposal");
    approve_creation(&f, &j, &create).await;
    std::fs::remove_file(f.workspace.path().join("note.txt")).expect("delete original source");
    assert!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
            .await
            .is_err()
    );
    assert!(!f.workspace.path().join("generated.txt").exists());
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn create_agent_reopens_with_fresh_consent_and_never_reexecutes_committed_effect() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let consent_id = create.consent_id().to_owned();
    let requirement = j
        .agent_creation_requirement("alice", "run-1")
        .expect("owned immutable requirement")
        .expect("explicit creation");
    assert_eq!(requirement.consent_id, consent_id);
    assert_eq!(requirement.version, create.consent_version());
    assert_eq!(requirement.environment, create.environment());
    assert!(j.agent_creation_requirement("bob", "run-1").is_err());
    let model = ScriptedModel::new(vec![write_response(), final_response()]);
    let (_stop, rx) = watch::channel(false);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("proposal");
    approve_creation(&f, &j, &create).await;
    drop(j);
    let mut j = f.open();
    assert!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
            .await
            .is_err(),
        "stale host capability rejected"
    );
    let create = j
        .restore_file_creation("alice", &consent_id)
        .expect("fresh host capability");
    j.connection.execute_batch("CREATE TRIGGER fail_context BEFORE UPDATE OF context_applied ON operations BEGIN SELECT RAISE(ABORT,'injected context failure'); END;").expect("inject context failure");
    assert!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
            .await
            .is_err()
    );
    let (state, applied): (String, i64) = j
        .connection
        .query_row("SELECT state,context_applied FROM operations", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("committed effect");
    assert_eq!((state.as_str(), applied), ("succeeded", 0));
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("generated.txt")).expect("published"),
        "actual created content"
    );
    j.connection
        .execute_batch("DROP TRIGGER fail_context;")
        .expect("repair injected fault");
    drop(j);
    let mut j = f.open();
    let create = j
        .restore_file_creation("alice", &consent_id)
        .expect("fresh host capability");
    let recovered = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("recover context only");
    assert_eq!(recovered.phase, Phase::Ready);
    assert_eq!(recovered.steps, 2);
    let done = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
        .await
        .expect("final response");
    assert_eq!(done.phase, Phase::Completed);
    assert_eq!(done.steps, 3);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let effects: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.succeeded'",
            [],
            |r| r.get(0),
        )
        .expect("effect count");
    assert_eq!(effects, 1);
    assert_eq!(
        model.requests.lock().expect("requests")[1]
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        1
    );
}

#[tokio::test]
async fn create_agent_postpublication_failure_quarantines_reopen_without_overwrite() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let consent_id = create.consent_id().to_owned();
    let model = ScriptedModel::new(vec![write_response()]);
    let (_stop, rx) = watch::channel(false);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("proposal");
    approve_creation(&f, &j, &create).await;
    j.connection.execute_batch("CREATE TRIGGER fail_receipt BEFORE UPDATE OF state ON operations WHEN NEW.state='succeeded' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").expect("fault");
    let stopped = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("unknown reported");
    assert_eq!(stopped.phase, Phase::RecoveryRequired);
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("generated.txt"))
            .expect("published before receipt failure"),
        "actual created content"
    );
    j.connection
        .execute_batch("DROP TRIGGER fail_receipt;")
        .expect("repair injected fault");
    drop(j);
    let mut j = f.open();
    assert!(j.restore_file_creation("alice", &consent_id).is_err());
    assert!(
        j.authorize_file_creation(
            "alice",
            "bypass-unknown",
            &f.config.policy,
            f.workspace.path()
        )
        .is_err()
    );
    std::fs::write(
        f.workspace.path().join("generated.txt"),
        "subsequent user edit",
    )
    .expect("edit owned fixture");
    let recovered = j
        .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
        .await
        .expect("metadata-only recovery view");
    assert_eq!(recovered.phase, Phase::RecoveryRequired);
    assert!(recovered.final_text.is_none() && recovered.pending.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("generated.txt")).expect("no replay"),
        "subsequent user edit"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let unknown: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM operations WHERE state='unknown'",
            [],
            |r| r.get(0),
        )
        .expect("unknown count");
    assert_eq!(unknown, 1);
}

#[tokio::test]
async fn create_agent_cancel_and_step_limit_have_no_file_effect() {
    for cancel in [true, false] {
        let mut f = Fixture::new();
        if !cancel {
            f.config.max_steps = 1;
        }
        let mut j = f.open();
        let create = creating(&f, &mut j);
        let model = ScriptedModel::new(vec![write_response()]);
        let (stop, rx) = watch::channel(false);
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
            .await
            .expect("proposal");
        approve_creation(&f, &j, &create).await;
        if cancel {
            stop.send(true).expect("cancel");
        }
        let view = j
            .advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
            .await
            .expect("stopped");
        assert_eq!(
            view.phase,
            if cancel {
                Phase::Cancelled
            } else {
                Phase::Limited
            }
        );
        assert!(!f.workspace.path().join("generated.txt").exists());
        assert_eq!(view.steps, 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn create_agent_schema_five_migration_never_upgrades_existing_run() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let create = j
        .authorize_file_creation(
            "alice",
            "independent-consent",
            &f.config.policy,
            f.workspace.path(),
        )
        .expect("separate consent");
    let id = create.consent_id().to_owned();
    j.connection
        .execute_batch("DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; DROP TABLE runtime_tool_approvals; DROP TABLE runtime_tool_catalogs; DROP TABLE runtime_catalog_legacy_approvals; DROP TABLE runtime_catalog_legacy; DROP TABLE runtime_creation_capabilities; PRAGMA user_version=5;")
        .expect("actual prior schema fixture");
    drop(j);
    let mut j = f.open();
    let version: i64 = j
        .connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, 8);
    assert_eq!(
        j.agent_view("alice", "run-1").expect("old run").phase,
        Phase::Ready
    );
    let restored = j
        .restore_file_creation("alice", &id)
        .expect("independent consent retained");
    assert!(
        j.start_agent_creating(
            &f.config,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            &f.grant,
            &restored,
            false
        )
        .is_err()
    );
    let count: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM runtime_creation_capabilities",
            [],
            |r| r.get(0),
        )
        .expect("binding count");
    assert_eq!(count, 0);
    let model = ScriptedModel::new(vec![final_response()]);
    let (_stop, rx) = watch::channel(false);
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("legacy run works")
            .phase,
        Phase::Completed
    );
    assert!(
        !model.requests.lock().expect("request")[0]
            .tools
            .iter()
            .any(|t| t.name == "write_file")
    );
}
