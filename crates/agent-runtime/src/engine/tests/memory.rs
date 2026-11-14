use super::*;
use crate::{
    catalog::ToolCatalog,
    memory::{MemoryGrant, MemoryMutation, MemoryOrigin, MemoryWriteRequest, tools::MemoryRequest},
};
fn scope(j: &mut Journal) -> MemoryGrant {
    j.create_memory_scope("alice", "personal", false)
        .expect("scope");
    j.authorize_memory("alice", "personal", "policy-v1", true)
        .expect("explicit memory grant")
}
fn prompt() -> Vec<Message> {
    vec![Message::plain(
        Role::User,
        "Remember my preference only after I review it.".into(),
    )]
}
fn write_call() -> ResponseCandidate {
    tools(vec![call(
        "write-memory",
        "memory_write",
        json!({"scope_id":"personal","mutation":"put","payload":{"title":"Preference","content":"Jasmine tea","enabled":true}}),
    )])
}
#[tokio::test]
async fn agent_memory_requires_committed_intent_exact_approval_and_revalidates_deleted_context() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![write_call()]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), authority, false)
        .expect("start");
    let view = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("propose");
    assert_eq!(view.phase, Phase::ToolsPending);
    let op = &view.pending[0];
    assert!(matches!(
        op.request,
        AgentRequest::Memory(MemoryRequest::MemoryWrite(_))
    ));
    assert!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &op.operation_id,
            now().expect("time") + 60,
            &f.grant
        )
        .is_err()
    );
    assert!(
        j.advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
            .await
            .is_err()
    );
    assert!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("empty facts")
            .view(&j)
            .expect("view")
            .entries
            .is_empty()
    );
    let preview = j
        .agent_memory_preview("alice", "run-1", &op.operation_id, authority)
        .await
        .expect("review actual draft");
    assert_eq!(preview.review.origin, MemoryOrigin::ModelInference);
    assert_eq!(preview.review.source_run, "run-1");
    assert_eq!(
        preview.review.payload.as_ref().expect("payload").content,
        "Jasmine tea"
    );
    let entry = preview.review.entry_id.clone();
    assert_eq!(
        model.requests.lock().expect("requests")[0]
            .tools
            .iter()
            .find(|v| v.name == "memory_write")
            .expect("visible granted scopes")
            .parameters["properties"]["scope_id"]["enum"],
        json!(["personal"])
    );
    let direct = j
        .review_memory_draft(&grants[0], &preview.review.draft_id)
        .expect("management may inspect own draft");
    assert!(
        j.approve_memory_write(direct, &preview.review.digest)
            .is_err(),
        "runtime-bound draft cannot bypass operation approval"
    );
    assert!(
        j.approve_agent_memory(
            "alice",
            "run-1",
            AgentMemoryApproval {
                operation_id: &op.operation_id,
                preview_digest: &"0".repeat(64),
                expires_at: now().expect("time") + 60
            },
            authority
        )
        .await
        .is_err()
    );
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &op.operation_id,
            preview_digest: &preview.review.digest,
            expires_at: now().expect("time") + 60,
        },
        authority,
    )
    .await
    .expect("explicit exact approval");
    assert!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("still pending execution")
            .view(&j)
            .expect("view")
            .entries
            .is_empty()
    );
    let view = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("execute memory write");
    assert_eq!(view.phase, Phase::Ready);
    assert_eq!(
        j.agent_memory_outcome("alice", "run-1", &op.operation_id, authority)
            .expect("lost reply metadata")
            .expect("outcome")
            .state,
        "approved"
    );
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(tools(vec![call(
            "read-memory",
            "memory_read",
            json!({"scope_id":"personal","entry_id":entry,"expected_version":"1"}),
        )]));
    let pending = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("read proposal");
    let read = &pending.pending[0];
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &read.operation_id,
            preview_digest: &read.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        authority,
    )
    .await
    .expect("read approval");
    j.advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("actual read");
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    let done = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("final model");
    assert_eq!(done.phase, Phase::Completed);
    assert!(done.final_text.is_some());
    assert!(
        j.agent_view("alice", "run-1")
            .expect("metadata only")
            .final_text
            .is_none()
    );
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
    let request = model
        .requests
        .lock()
        .expect("requests")
        .last()
        .expect("last")
        .clone();
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.contains("Jasmine tea"))
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    j.register_run("alice", "delete-run", "policy-v1")
        .expect("user management");
    let p = j
        .prepare_memory_write(
            &grants[0],
            "delete",
            MemoryWriteRequest {
                entry_id: entry,
                expected_version: Some("1".into()),
                source_run: "delete-run".into(),
                origin: MemoryOrigin::UserDeclaration,
                mutation: MemoryMutation::Delete,
                payload: None,
            },
        )
        .expect("delete preview");
    let d = p.view(&j).expect("review").digest.clone();
    j.approve_memory_write(p, &d).expect("delete");
    assert!(
        j.agent_view_with_memory("alice", "run-1", &f.grant, &grants)
            .await
            .is_err()
    );
    assert!(
        j.advance_agent_authorized("alice", "run-1", authority, &model, rx)
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    assert!(
        j.agent_view("alice", "run-1")
            .expect("redacted metadata")
            .final_text
            .is_none()
    );
}
#[tokio::test]
async fn absent_readonly_or_forged_memory_authority_never_creates_an_operation() {
    for mode in ["absent", "read-only", "forged", "ungranted-scope"] {
        let f = Fixture::new();
        let mut j = f.open();
        let writer = scope(&mut j);
        let grants = if mode == "absent" {
            vec![]
        } else if mode == "read-only" {
            vec![
                j.authorize_memory("alice", "personal", "policy-v1", false)
                    .expect("read-only"),
            ]
        } else {
            vec![writer]
        };
        let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
        let mut response = write_call();
        if mode == "forged" {
            response.tool_calls[0].arguments["origin"] = json!("user_declaration");
        }
        if mode == "ungranted-scope" {
            response.tool_calls[0].arguments["scope_id"] = json!("hidden");
        }
        let model = ScriptedModel::new(vec![response]);
        let (_tx, rx) = watch::channel(false);
        j.start_agent_authorized(&f.config, prompt(), authority, false)
            .expect("start");
        assert!(
            j.advance_agent_authorized("alice", "run-1", authority, &model, rx)
                .await
                .is_err(),
            "{mode}"
        );
        let count: i64 = j
            .connection
            .query_row("SELECT count(*) FROM operations", [], |r| r.get(0))
            .expect("effects");
        assert_eq!(count, 0, "{mode}");
        let count: i64 = j
            .connection
            .query_row("SELECT count(*) FROM memory_drafts", [], |r| r.get(0))
            .expect("drafts");
        assert_eq!(count, 0, "{mode}");
    }
}
#[test]
fn generic_request_preserves_workspace_hash_and_memory_descriptors_reject_forged_fields() {
    let raw = json!({"path":"note.txt"});
    let old = WorkspaceRequest::parse("read_file", &raw).expect("old");
    let generic = AgentRequest::parse("read_file", &raw).expect("generic");
    assert_eq!(old.digest().expect("hash"), generic.digest().expect("hash"));
    assert_eq!(
        serde_json::to_value(old).expect("json"),
        serde_json::to_value(generic).expect("json")
    );
    let catalog = ToolCatalog::builtin_memory(true, true, true, true).expect("catalog");
    let pin = catalog.pin("memory_write").expect("pin");
    assert!(
        catalog
            .validate_input(&pin, &write_call().tool_calls[0].arguments)
            .is_ok()
    );
    let mut forged = write_call().tool_calls[0].arguments.clone();
    forged["actor"] = json!("admin");
    assert!(catalog.validate_input(&pin, &forged).is_err());
    assert!(MemoryRequest::parse("memory_write", &forged).is_err());
    for invalid in [
        json!({"scope_id":"personal","mutation":"delete"}),
        json!({"scope_id":"personal","mutation":"put"}),
        json!({"scope_id":"personal","entry_id":"id","mutation":"put","payload":{"title":"x","content":"x","enabled":true}}),
        json!({"scope_id":"personal","entry_id":"id","expected_version":"1","mutation":"delete","payload":{"title":"x","content":"x","enabled":true}}),
    ] {
        assert!(catalog.validate_input(&pin, &invalid).is_err());
        assert!(MemoryRequest::parse("memory_write", &invalid).is_err());
    }

    let memory = AgentRequest::parse(
        "memory_read",
        &json!({"scope_id":"personal","entry_id":"entry"}),
    )
    .expect("memory");
    assert!(memory.workspace().is_err());
    assert!(
        ToolCatalog::builtin(false, false)
            .expect("old catalog")
            .pin("memory_read")
            .is_err()
    );
}

#[tokio::test]
async fn memory_revocation_cancels_inflight_model_without_committing_response_or_intent() {
    struct Waiting {
        entered: tokio::sync::Notify,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    struct Dropped(
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        watch::Receiver<bool>,
    );
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(*self.1.borrow(), Ordering::SeqCst);
        }
    }
    impl ModelGateway for Waiting {
        fn generate<'a>(
            &'a self,
            _: &'a GenerateRequest,
            cancel: watch::Receiver<bool>,
        ) -> ModelFuture<'a> {
            Box::pin(async move {
                let _guard = Dropped(self.dropped.clone(), cancel);
                self.entered.notify_one();
                std::future::pending::<()>().await;
                Err(GatewayFailure)
            })
        }
    }
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
    j.start_agent_authorized(&f.config, prompt(), authority, false)
        .expect("start");
    let model = Waiting {
        entered: tokio::sync::Notify::new(),
        dropped: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (_tx, rx) = watch::channel(false);
    let work = j.advance_agent_authorized("alice", "run-1", authority, &model, rx);
    let revoke = async {
        model.entered.notified().await;
        grants[0].request_revoke();
    };
    let (result, _) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(work, revoke) })
            .await
            .expect("revocation stops await");
    assert!(result.is_err());
    assert!(model.dropped.load(Ordering::SeqCst));
    for table in ["responses", "operations", "memory_drafts"] {
        let count: i64 = j
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("no commit");
        assert_eq!(count, 0, "{table}");
    }
    assert_eq!(
        j.agent_view("alice", "run-1").expect("metadata").phase,
        Phase::RecoveryRequired
    );
}
#[tokio::test]
async fn late_approval_loss_rolls_back_memory_fact_and_never_replays_unknown_operation() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![write_call()]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), authority, false)
        .expect("start");
    let view = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("intent");
    let op = &view.pending[0];
    let preview = j
        .agent_memory_preview("alice", "run-1", &op.operation_id, authority)
        .await
        .expect("preview");
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &op.operation_id,
            preview_digest: &preview.review.digest,
            expires_at: now().expect("time") + 60,
        },
        authority,
    )
    .await
    .expect("approve");
    j.connection.execute_batch("CREATE TRIGGER memory_late_approval_loss AFTER INSERT ON memory_entries BEGIN DELETE FROM approvals;END;").expect("owned race fixture");
    assert!(
        j.advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
            .await
            .is_err()
    );
    assert!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("facts")
            .view(&j)
            .expect("view")
            .entries
            .is_empty()
    );
    assert_eq!(
        j.agent_memory_outcome("alice", "run-1", &op.operation_id, authority)
            .expect("outcome")
            .expect("draft")
            .state,
        "pending"
    );
    assert_eq!(
        j.state("alice", &op.operation_id)
            .expect("state")
            .as_deref(),
        Some("unknown")
    );
    j.connection
        .execute_batch("DROP TRIGGER memory_late_approval_loss;")
        .expect("remove owned fixture");
    j.advance_agent_authorized("alice", "run-1", authority, &model, rx)
        .await
        .expect("metadata only");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("no replay")
            .view(&j)
            .expect("view")
            .entries
            .is_empty()
    );
}

#[test]
fn memory_catalogue_matches_fixed_execution_pins_without_changing_legacy_pins() {
    assert!(
        ToolCatalog::builtin_memory_scoped(false, false, false, &[])
            .expect("no scopes")
            .pin("memory_read")
            .is_err()
    );
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../contracts/fixtures/memory-tool-pins.v1.json"
    ))
    .expect("fixed pins");
    let all = ToolCatalog::builtin_memory(true, true, true, true).expect("all capabilities");
    assert_eq!(
        expected["pins"].as_object().expect("pins").len(),
        all.document().tools.len()
    );
    for spec in all.document().tools {
        assert_eq!(
            expected["pins"][&spec.tool_version].as_str(),
            Some(spec.execution_digest().expect("digest").as_str())
        );
    }
    let old = ToolCatalog::builtin_editing(true, true).expect("existing editing tools");
    for spec in old.document().tools {
        assert_eq!(
            old.pin(&spec.name).expect("old pin"),
            all.pin(&spec.name).expect("unchanged")
        );
    }
}

#[tokio::test]
async fn restored_memory_scope_must_be_explicit_and_old_metadata_api_never_returns_pending_content()
{
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![write_call()]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), authority, false)
        .expect("start");
    let view = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("propose");
    let op = view.pending[0].operation_id.clone();
    let expected = j
        .agent_memory_requirements("alice", "run-1")
        .expect("safe metadata");
    assert_eq!(expected.len(), 1);
    assert!(
        j.agent_view("alice", "run-1")
            .expect("safe default")
            .pending
            .is_empty()
    );
    drop(j);
    let mut j = f.open();
    assert!(
        j.agent_view_with_memory("alice", "run-1", &f.grant, &grants)
            .await
            .is_err()
    );
    assert!(
        j.advance_agent("alice", "run-1", &f.grant, &model, rx.clone())
            .await
            .is_err()
    );
    let fresh = vec![
        j.authorize_memory("alice", "personal", "policy-v1", true)
            .expect("explicit current permission"),
    ];
    assert_eq!(
        j.agent_memory_requirements("alice", "run-1")
            .expect("requirements"),
        expected
    );
    let authority = AgentAuthority::read(&f.grant).with_memory(&fresh);
    let shown = j
        .agent_view_with_memory("alice", "run-1", &f.grant, &fresh)
        .await
        .expect("authorized restored view");
    assert_eq!(shown.pending[0].operation_id, op);
    let preview = j
        .agent_memory_preview("alice", "run-1", &op, authority)
        .await
        .expect("fresh review");
    assert!(
        j.advance_agent_authorized("alice", "run-1", authority, &model, rx)
            .await
            .is_err(),
        "restoring access is not approval"
    );
    j.cancel_agent_run("alice", "run-1")
        .expect("cancel without data access");
    assert_eq!(
        j.memory_draft_outcome(&fresh[0], &preview.review.draft_id)
            .expect("outcome")
            .expect("draft")
            .state,
        "invalidated"
    );
}
#[tokio::test]
async fn memory_scoped_user_reply_is_context_and_never_a_memory_approval() {
    use orange_protocol::questions::AnswerUser;
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let authority = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "question",
            "ask_user",
            json!({"questions":[{"prompt":"Which preference?","choices":["Tea","Coffee"]}]}),
        )]),
        write_call(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), authority, true)
        .expect("start");
    let waiting = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("ask");
    let question = waiting.user_question.expect("question");
    let answer = AnswerUser {
        question_id: question.question_id,
        argument_digest: question.argument_digest,
        answers: vec!["Tea".into()],
    };
    assert!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx.clone())
            .await
            .is_err()
    );
    j.answer_agent_question_authorized("alice", "run-1", &answer, authority, rx.clone())
        .await
        .expect("explicit scoped reply");
    let pending = j
        .advance_agent_authorized("alice", "run-1", authority, &model, rx.clone())
        .await
        .expect("next proposal");
    assert_eq!(pending.pending.len(), 1);
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM approvals", [], |r| r.get(0))
        .expect("no permission upgrade");
    assert_eq!(count, 0);
    assert!(
        j.advance_agent_authorized("alice", "run-1", authority, &model, rx)
            .await
            .is_err()
    );
    assert!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("facts")
            .view(&j)
            .expect("view")
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn ambiguous_memory_effect_is_quarantined_and_requires_exact_explicit_resolution() {
    for applied in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        let grants = vec![scope(&mut j)];
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        let model = ScriptedModel::new(vec![write_call()]);
        let (_tx, rx) = watch::channel(false);
        j.start_agent_authorized(&f.config, prompt(), auth, false)
            .expect("start");
        let view = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("intent");
        let op = view.pending[0].operation_id.clone();
        let preview = j
            .agent_memory_preview("alice", "run-1", &op, auth)
            .await
            .expect("preview");
        j.approve_agent_memory(
            "alice",
            "run-1",
            AgentMemoryApproval {
                operation_id: &op,
                preview_digest: &preview.review.digest,
                expires_at: now().expect("time") + 60,
            },
            auth,
        )
        .await
        .expect("approve");
        if applied {
            j.connection.execute_batch("CREATE TRIGGER unknown_memory_result BEFORE UPDATE ON operations WHEN NEW.state='succeeded' AND NEW.tool_version='memory_write.v1' BEGIN SELECT RAISE(ABORT,'owned result fault');END;").expect("fault");
        } else {
            j.connection.execute_batch("CREATE TRIGGER unknown_memory_result AFTER INSERT ON memory_entries BEGIN DELETE FROM approvals;END;").expect("fault");
        }
        assert!(
            j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
                .await
                .is_err()
        );
        let outcome = j
            .agent_memory_metadata("alice", "run-1", &op)
            .expect("owned metadata")
            .expect("draft");
        assert_eq!(outcome.state, if applied { "approved" } else { "pending" });
        assert!(j.agent_memory_metadata("bob", "run-1", &op).is_err());
        let mut other = f.config.clone();
        other.run_id = "new-run".into();
        assert!(
            matches!(
                j.start_agent_authorized(&other, prompt(), auth, false),
                Err(EngineError::RecoveryRequired)
            ),
            "a new Run cannot bypass an unresolved memory write"
        );
        j.connection
            .execute_batch("DROP TRIGGER unknown_memory_result;")
            .expect("remove fault");
        assert!(
            j.reconcile_agent_memory(
                "alice",
                "run-1",
                &op,
                &"0".repeat(64),
                MemoryRecovery::ConfirmAppliedAndStop
            )
            .await
            .is_err()
        );
        let wrong = if applied {
            MemoryRecovery::ConfirmNotAppliedAndStop
        } else {
            MemoryRecovery::ConfirmAppliedAndStop
        };
        assert!(
            j.reconcile_agent_memory("alice", "run-1", &op, &outcome.digest, wrong)
                .await
                .is_err()
        );
        model
            .responses
            .lock()
            .expect("queue")
            .push_back(final_response());
        let decision = if applied {
            MemoryRecovery::ConfirmAppliedAndContinue(auth)
        } else {
            MemoryRecovery::ConfirmNotAppliedAndContinue(auth)
        };
        let resumed = j
            .reconcile_agent_memory("alice", "run-1", &op, &outcome.digest, decision)
            .await
            .expect("exact witnessed resolution");
        assert_eq!(resumed.phase, Phase::Ready);
        assert!(
            j.reconcile_agent_memory(
                "alice",
                "run-1",
                &op,
                &outcome.digest,
                MemoryRecovery::ConfirmAppliedAndContinue(auth)
            )
            .await
            .is_err(),
            "no repeat resolution"
        );
        let done = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("continue with result, not effect replay");
        assert_eq!(done.phase, Phase::Completed);
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        let facts = j.list_memory(&grants[0], None, 100, false).expect("facts");
        assert_eq!(
            facts.view(&j).expect("view").entries.len(),
            usize::from(applied)
        );
        assert_eq!(
            j.state("alice", &op).expect("state").as_deref(),
            Some(if applied { "succeeded" } else { "failed" })
        );
    }
}
#[tokio::test]
async fn confirmed_memory_effect_can_be_finished_without_restoring_scope_or_workspace_access() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![write_call()]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), auth, false)
        .expect("start");
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("intent");
    let op = view.pending[0].operation_id.clone();
    let preview = j
        .agent_memory_preview("alice", "run-1", &op, auth)
        .await
        .expect("preview");
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &op,
            preview_digest: &preview.review.digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.connection.execute_batch("CREATE TRIGGER result_fault BEFORE UPDATE ON operations WHEN NEW.state='succeeded' AND NEW.tool_version='memory_write.v1' BEGIN SELECT RAISE(ABORT,'owned fault');END;").expect("fault");
    assert!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .is_err()
    );
    j.connection
        .execute_batch("DROP TRIGGER result_fault;")
        .expect("remove fault");
    let outcome = j
        .agent_memory_metadata("alice", "run-1", &op)
        .expect("metadata")
        .expect("receipt");
    assert_eq!(outcome.state, "approved");
    grants[0].request_revoke();
    f.grant.request_revoke();
    j.set_memory_scope_enabled("alice", "personal", "1", false)
        .expect("user disabled scope");
    let done = j
        .reconcile_agent_memory(
            "alice",
            "run-1",
            &op,
            &outcome.digest,
            MemoryRecovery::ConfirmAppliedAndStop,
        )
        .await
        .expect("owned metadata only");
    assert_eq!(done.phase, Phase::Cancelled);
    assert!(done.final_text.is_none());
    assert!(done.pending.is_empty());
    assert!(
        !j.memory_scope("alice", "personal")
            .expect("scope metadata")
            .enabled
    );
    assert!(
        j.authorize_memory("alice", "personal", "policy-v1", false)
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        j.state("alice", &op).expect("record").as_deref(),
        Some("succeeded")
    );
}
#[tokio::test]
async fn absent_memory_is_a_bounded_read_failure_without_a_false_effect_or_unknown_write() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![scope(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "missing",
            "memory_read",
            json!({"scope_id":"personal","entry_id":"absent"}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(&f.config, prompt(), auth, false)
        .expect("start");
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("intent");
    let op = &view.pending[0];
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &op.operation_id,
            preview_digest: &op.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve read");
    let result = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("controlled read failure");
    assert_eq!(result.phase, Phase::Ready);
    assert_eq!(
        j.state("alice", &op.operation_id)
            .expect("state")
            .as_deref(),
        Some("failed")
    );
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
        .await
        .expect("continue using error");
    let requests = model.requests.lock().expect("requests");
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content == "{\"error\":\"memory_unavailable\"}")
    );
    assert_eq!(
        j.list_memory(&grants[0], None, 100, false)
            .expect("no effects")
            .view(&j)
            .expect("view")
            .entries
            .len(),
        0
    );
}
