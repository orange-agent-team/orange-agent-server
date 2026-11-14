use super::*;
fn read_proposal() -> ResponseCandidate {
    tools(vec![call("read", "read_file", json!({"path":"note.txt"}))])
}
fn count(j: &Journal, table: &str) -> i64 {
    j.connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("fixture count")
}
fn downgrade(j: &Journal) {
    j.connection.execute_batch("DROP TABLE export_retirement_capsules; DROP TABLE export_retirements; DROP TABLE export_writer_bindings; DROP TABLE patch_exports; DROP TABLE patch_capture_receipts; DROP TABLE patch_capture_witnesses; DROP TABLE runtime_edit_capabilities; DROP TABLE patch_version_edges; DROP TABLE patch_retention_archives; DROP TABLE patch_retention_reviews; DROP TABLE patch_recovery_resolutions; DROP TABLE patch_recovery_results; DROP TABLE patch_recovery_approvals; DROP TABLE patch_recoveries; DROP TABLE editing_publication_steps; DROP TABLE editing_publications; DROP TABLE editing_approvals; DROP TABLE editing_previews; DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; DROP TABLE runtime_tool_approvals; DROP TABLE runtime_tool_catalogs; DROP TABLE runtime_catalog_legacy_approvals; DROP TABLE runtime_catalog_legacy; PRAGMA user_version=6;").expect("actual pre-catalog schema fixture");
}
#[tokio::test]
async fn catalogue_capture_failure_rolls_back_model_dispatch_and_run_counters() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    j.connection.execute_batch("CREATE TRIGGER fail_catalog BEFORE INSERT ON runtime_tool_catalogs BEGIN SELECT RAISE(ABORT,'injected snapshot failure');END;").expect("fault");
    let model = ScriptedModel::new(vec![read_proposal()]);
    let (_stop, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&j, "runtime_invocations"), 0);
    assert_eq!(count(&j, "runtime_tool_catalogs"), 0);
    assert_eq!(count(&j, "responses"), 0);
    let view = j.agent_view("alice", "run-1").expect("view");
    assert_eq!(view.phase, Phase::Ready);
    assert_eq!((view.steps, view.invocations), (0, 0));
}
#[tokio::test]
async fn approval_contract_failure_is_atomic_and_generic_approval_cannot_bypass_pin() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![read_proposal()]);
    let (_stop, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("proposal");
    let stored = j.stored_run("alice", "run-1").expect("run");
    let op = j.pending_operations("run-1").expect("ops").remove(0);
    j.connection.execute_batch("CREATE TRIGGER fail_pin BEFORE INSERT ON runtime_tool_approvals BEGIN SELECT RAISE(ABORT,'injected approval pin failure');END;").expect("fault");
    assert!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &op.id,
            now().expect("time") + 60,
            &f.grant
        )
        .is_err()
    );
    assert_eq!(count(&j, "approvals"), 0);
    assert_eq!(count(&j, "runtime_tool_approvals"), 0);
    j.connection
        .execute_batch("DROP TRIGGER fail_pin;")
        .expect("repair");
    j.approve(
        &op.operation(&stored.config),
        "alice",
        now().expect("time") + 60,
    )
    .expect("low-level legacy approval");
    assert!(matches!(
        f.advance(&mut j, &model, rx.clone()).await,
        Err(EngineError::ApprovalRequired)
    ));
    assert_eq!(
        j.state("alice", &op.id).expect("state").as_deref(),
        Some("intended")
    );
    f.approve(&j);
    assert_eq!(count(&j, "runtime_tool_approvals"), 1);
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("approved read")
            .phase,
        Phase::Ready
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn new_missing_catalog_never_becomes_legacy_or_visible_to_authorized_ui() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![read_proposal()]);
    let (_stop, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("proposal");
    assert_eq!(count(&j, "runtime_catalog_legacy"), 0);
    j.connection
        .execute_batch("DROP TRIGGER runtime_catalog_no_delete; DELETE FROM runtime_tool_catalogs;")
        .expect("explicit corruption fixture");
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
    let op = j.pending_operations("run-1").expect("ops").remove(0);
    assert!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &op.id,
            now().expect("time") + 60,
            &f.grant
        )
        .is_err()
    );
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    assert_eq!(count(&j, "runtime_tool_catalogs"), 0);
    assert_eq!(count(&j, "approvals"), 0);
    assert_eq!(
        j.state("alice", &op.id).expect("state").as_deref(),
        Some("intended")
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn schema_six_request_and_exact_approval_are_adopted_once_without_new_rights() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![read_proposal(), final_response()]);
    let (_stop, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("proposal");
    f.approve(&j);
    downgrade(&j);
    drop(j);
    let mut j = f.open();
    assert_eq!(count(&j, "runtime_catalog_legacy"), 1);
    assert_eq!(count(&j, "runtime_catalog_legacy_approvals"), 1);
    assert_eq!(count(&j, "runtime_tool_catalogs"), 0);
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("exact legacy approval")
            .phase,
        Phase::Ready
    );
    let origin: String = j
        .connection
        .query_row("SELECT origin FROM runtime_tool_catalogs", [], |r| r.get(0))
        .expect("origin");
    assert_eq!(origin, "legacy_builtin_v1");
    assert_eq!(count(&j, "runtime_tool_approvals"), 1);
    assert!(
        j.agent_creation_requirement("alice", "run-1")
            .expect("requirement")
            .is_none()
    );
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("final")
            .phase,
        Phase::Completed
    );
    drop(j);
    let mut j = f.open();
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("no replay")
            .phase,
        Phase::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(count(&j, "runtime_tool_catalogs"), 2);
    let effects: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.succeeded'",
            [],
            |r| r.get(0),
        )
        .expect("effects");
    assert_eq!(effects, 1);
}
#[tokio::test]
async fn schema_six_changed_schema_or_changed_approval_is_not_silently_adopted() {
    for change_schema in [true, false] {
        let f = Fixture::new();
        let mut j = f.open();
        f.start(&mut j);
        let model = ScriptedModel::new(vec![read_proposal()]);
        let (_stop, rx) = watch::channel(false);
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("proposal");
        f.approve(&j);
        downgrade(&j);
        if change_schema {
            let (invocation, raw): (String, String) = j
                .connection
                .query_row(
                    "SELECT invocation,request FROM runtime_invocations",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("request");
            let mut request: GenerateRequest = serde_json::from_str(&raw).expect("request");
            request.tools[0].parameters = json!({"type":"object"});
            j.connection
                .execute(
                    "UPDATE runtime_invocations SET request=?,request_digest=? WHERE invocation=?",
                    params![
                        encode(&request).expect("request"),
                        digest("model-request-v1", &request).expect("digest"),
                        invocation
                    ],
                )
                .expect("old unknown schema fixture");
        }
        drop(j);
        let mut j = f.open();
        if !change_schema {
            j.connection
                .execute("UPDATE approvals SET expires_at=expires_at+60", [])
                .expect("new unbound approval after migration");
        }
        assert!(f.advance(&mut j, &model, rx).await.is_err());
        assert_eq!(count(&j, "runtime_tool_approvals"), 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let effects: i64 = j
            .connection
            .query_row(
                "SELECT count(*) FROM operations WHERE state!='intended'",
                [],
                |r| r.get(0),
            )
            .expect("effects");
        assert_eq!(effects, 0);
    }
}
#[tokio::test]
async fn snapshot_and_approval_are_immutable_and_bad_cached_output_never_reaches_model() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![read_proposal(), final_response()]);
    let (_stop, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("proposal");
    f.approve(&j);
    assert!(
        j.connection
            .execute(
                "UPDATE runtime_tool_catalogs SET revision=?",
                ["a".repeat(64)]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM runtime_tool_catalogs", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "UPDATE runtime_tool_approvals SET tool_version='read_file.v2'",
                []
            )
            .is_err()
    );
    j.connection.execute_batch("CREATE TRIGGER pause_context BEFORE UPDATE OF context_applied ON operations BEGIN SELECT RAISE(ABORT,'context fault');END;").expect("fault");
    assert!(f.advance(&mut j, &model, rx.clone()).await.is_err());
    let original: String = j
        .connection
        .query_row("SELECT model_result FROM operations", [], |r| r.get(0))
        .expect("committed result");
    j.connection.execute_batch("DROP TRIGGER pause_context; UPDATE operations SET model_result='{\"kind\":\"read_file\",\"data\":{\"content\":\"untrusted cached output\"}}';").expect("corrupt output fixture");
    assert!(f.advance(&mut j, &model, rx.clone()).await.is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        j.connection
            .query_row("SELECT context_applied FROM operations", [], |r| r
                .get::<_, i64>(0))
            .expect("not applied"),
        0
    );
    let mut plausible: serde_json::Value =
        serde_json::from_str(&original).expect("valid structure");
    plausible["data"]["content"] = json!("altered data");
    let stored = j.stored_run("alice", "run-1").expect("run");
    let op = j.pending_operations("run-1").expect("op").remove(0);
    let (catalog, pin) = j
        .intent_contract(&stored.config, &op)
        .expect("intact contract");
    catalog
        .validate_output(&pin, &plausible, crate::catalog::OutputChannel::Model)
        .expect("same-shaped corruption is schema-valid");
    j.connection
        .execute(
            "UPDATE operations SET model_result=?",
            [encode(&plausible).expect("payload")],
        )
        .expect("same-shaped corruption");
    assert!(
        f.advance(&mut j, &model, rx.clone()).await.is_err(),
        "schema validity cannot replace actual committed result provenance"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    j.connection
        .execute("UPDATE operations SET model_result=?", [original])
        .expect("repair fixture");
    let original_raw: String = j
        .connection
        .query_row("SELECT result_json FROM operations", [], |r| r.get(0))
        .expect("raw receipt");
    let mut raw: serde_json::Value = serde_json::from_str(&original_raw).expect("raw JSON");
    raw["grant_access"] = json!(true);
    j.connection
        .execute(
            "UPDATE operations SET result_json=?",
            [encode(&raw).expect("raw JSON")],
        )
        .expect("unknown durable result field");
    assert!(
        f.advance(&mut j, &model, rx.clone()).await.is_err(),
        "typed deserialization must not hide unrecognized durable-result fields"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    j.connection
        .execute("UPDATE operations SET result_json=?", [original_raw])
        .expect("restore raw receipt");
    assert_eq!(
        f.advance(&mut j, &model, rx.clone())
            .await
            .expect("recover context")
            .phase,
        Phase::Ready
    );
    assert_eq!(
        f.advance(&mut j, &model, rx)
            .await
            .expect("model after valid context")
            .phase,
        Phase::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn generic_creation_entry_cannot_bypass_normal_run_read_authority_or_contract_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = creating(&f, &mut j);
    let model = ScriptedModel::new(vec![write_response()]);
    let (_stop, rx) = watch::channel(false);
    j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx.clone())
        .await
        .expect("proposal");
    let stored = j.stored_run("alice", "run-1").expect("run");
    let op = j.pending_operations("run-1").expect("op").remove(0);
    let WorkspaceRequest::WriteFile(args) = op.request.clone() else {
        panic!("write proposal");
    };
    j.approve(
        &op.operation(&stored.config),
        "alice",
        now().expect("time") + 60,
    )
    .expect("generic approval");
    assert!(
        j.create_file(&create, &op.operation(&stored.config), args.clone())
            .await
            .is_err()
    );
    assert!(!f.workspace.path().join("generated.txt").exists());
    approve_creation(&f, &j, &create).await;
    assert!(
        j.create_file(&create, &op.operation(&stored.config), args)
            .await
            .is_err(),
        "normal Agent writes require paired read capability"
    );
    assert!(!f.workspace.path().join("generated.txt").exists());
    assert_eq!(
        j.advance_agent_creating("alice", "run-1", &f.grant, &create, &model, rx)
            .await
            .expect("proper execution entry")
            .phase,
        Phase::Ready
    );
}

#[tokio::test]
async fn changed_operation_version_cannot_reuse_a_valid_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    let model = ScriptedModel::new(vec![read_proposal()]);
    let (_stop, rx) = watch::channel(false);
    f.advance(&mut j, &model, rx.clone())
        .await
        .expect("proposal");
    f.approve(&j);
    j.connection
        .execute("UPDATE operations SET tool_version='read_file.v2'", [])
        .expect("simulate contract drift");
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
    assert_eq!(count(&j, "approvals"), 1);
    assert_eq!(count(&j, "runtime_tool_approvals"), 1);
    let dispatched: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.dispatching'",
            [],
            |r| r.get(0),
        )
        .expect("effects");
    assert_eq!(dispatched, 0);
}
#[tokio::test]
async fn schema_six_pending_question_recovers_its_exact_contract_without_tool_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    j.start_agent_interactive(
        &f.config,
        vec![Message::plain(Role::User, "Ask before reading".into())],
        &f.grant,
    )
    .expect("interactive opt-in");
    let model = ScriptedModel::new(vec![tools(vec![call(
        "question",
        "ask_user",
        json!({"questions":[{"prompt":"Which file?"}]}),
    )])]);
    let (_stop, rx) = watch::channel(false);
    let waiting = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("question");
    let question = waiting.user_question.expect("question data");
    downgrade(&j);
    drop(j);
    let mut j = f.open();
    let restored = j
        .agent_view_authorized("alice", "run-1", &f.grant)
        .await
        .expect("owned question");
    assert_eq!(
        restored.user_question.expect("question").question_id,
        question.question_id
    );
    let answer = orange_protocol::questions::AnswerUser {
        question_id: question.question_id,
        argument_digest: question.argument_digest,
        answers: vec!["note.txt".into()],
    };
    assert_eq!(
        j.answer_agent_question("alice", "run-1", &answer, &f.grant, rx)
            .await
            .expect("exact reply")
            .phase,
        Phase::Ready
    );
    assert_eq!(count(&j, "approvals"), 0);
    assert_eq!(count(&j, "runtime_tool_approvals"), 0);
    assert_eq!(count(&j, "runtime_tool_catalogs"), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}
