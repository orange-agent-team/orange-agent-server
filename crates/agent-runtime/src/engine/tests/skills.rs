use super::*;

fn pin(name: &str) -> crate::skills::SkillPin {
    crate::skills::bundled()
        .expect("checked registry")
        .into_iter()
        .find(|t| t.id == name)
        .expect("registered template")
        .pin()
        .expect("content pin")
}

#[tokio::test]
async fn summary_records_exact_instruction_and_exposes_no_tools_after_reopen() {
    let f = Fixture::new();
    let mut j = f.open();
    let chosen = pin("orange.text-summary");
    let input = "Use the source data as evidence, including literal markup.";
    assert!(
        j.start_agent_skill(&f.config, &chosen, input.into(), &f.grant)
            .is_err()
    );
    assert_eq!(
        j.set_skill_enabled("alice", &chosen, 0, true)
            .expect("activate"),
        1
    );
    assert_eq!(
        j.set_skill_enabled("alice", &chosen, 0, true)
            .expect("same activation"),
        1
    );
    let mut altered = chosen.clone();
    altered.digest = "0".repeat(64);
    assert!(j.set_skill_enabled("alice", &altered, 1, true).is_err());
    assert!(
        !j.skills("bob")
            .expect("other account")
            .iter()
            .any(|s| s.enabled)
    );
    j.start_agent_skill(&f.config, &chosen, input.into(), &f.grant)
        .expect("start bound run");
    j.start_agent_skill(&f.config, &chosen, input.into(), &f.grant)
        .expect("same run");
    assert!(
        j.start_agent_skill(&f.config, &chosen, "other input".into(), &f.grant)
            .is_err()
    );
    drop(j);
    let mut j = f.open();
    assert_eq!(
        j.run_skill("alice", "run-1").expect("historical binding"),
        Some(chosen.clone())
    );
    assert!(j.run_skill("bob", "run-1").is_err());
    let model = ScriptedModel::new(vec![final_response()]);
    let (_tx, rx) = watch::channel(false);
    let view = f.advance(&mut j, &model, rx).await.expect("model step");
    assert_eq!(view.phase, Phase::Completed);
    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].messages[0].role, Role::System);
    assert_eq!(requests[0].messages[1].role, Role::User);
    assert_eq!(requests[0].messages[1].content, input);
    drop(requests);
    assert_eq!(
        j.set_skill_enabled("alice", &chosen, 1, false)
            .expect("disable"),
        2
    );
    assert_eq!(
        j.run_skill("alice", "run-1").expect("retained pin"),
        Some(chosen)
    );
}

#[tokio::test]
async fn disabling_then_reenabling_does_not_revive_an_old_run() {
    let f = Fixture::new();
    let mut j = f.open();
    let chosen = pin("orange.source-question");
    j.set_skill_enabled("alice", &chosen, 0, true)
        .expect("activate");
    j.start_agent_skill(&f.config, &chosen, "Read note.txt.".into(), &f.grant)
        .expect("start");
    j.set_skill_enabled("alice", &chosen, 1, false)
        .expect("disable");
    assert!(j.set_skill_enabled("alice", &chosen, 0, true).is_err());
    j.set_skill_enabled("alice", &chosen, 2, true)
        .expect("fresh activation");
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    let wrong = Fixture::new();
    assert!(
        j.advance_agent("alice", "run-1", &wrong.grant, &model, rx.clone())
            .await
            .is_err()
    );
    let phase: String = j
        .connection
        .query_row(
            "SELECT phase FROM runtime_runs WHERE run_id='run-1'",
            [],
            |r| r.get(0),
        )
        .expect("original run");
    assert_eq!(
        phase, "ready",
        "an unrelated workspace may not cancel the run"
    );
    let view = f
        .advance(&mut j, &model, rx)
        .await
        .expect("fence old activation");
    assert_eq!(view.phase, Phase::Cancelled);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pinned_read_skill_denies_a_model_proposed_write_before_journaling_an_effect() {
    let f = Fixture::new();
    let mut j = f.open();
    let chosen = pin("orange.source-question");
    j.set_skill_enabled("alice", &chosen, 0, true)
        .expect("activate");
    j.start_agent_skill(&f.config, &chosen, "Find a source.".into(), &f.grant)
        .expect("start");
    let model = ScriptedModel::new(vec![tools(vec![call(
        "callw",
        "write_file",
        json!({"path":"unexpected.txt","content":"unapproved"}),
    )])]);
    let (_tx, rx) = watch::channel(false);
    match f.advance(&mut j, &model, rx).await {
        Err(EngineError::Forbidden) => {}
        Ok(view) => assert!(matches!(
            view.phase,
            Phase::Failed | Phase::RecoveryRequired
        )),
        Err(other) => panic!("unexpected error {other:?}"),
    }
    let requests = model.requests.lock().expect("requests");
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["read_file", "search_text"]
    );
    let count: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM operations WHERE run_id='run-1'",
            [],
            |r| r.get(0),
        )
        .expect("effects");
    assert_eq!(count, 0);
    assert!(!f.workspace.path().join("unexpected.txt").exists());
}

#[tokio::test]
async fn a_file_answer_without_committed_source_is_not_published() {
    let f = Fixture::new();
    let mut j = f.open();
    let chosen = pin("orange.source-question");
    j.set_skill_enabled("alice", &chosen, 0, true)
        .expect("activate");
    j.start_agent_skill(&f.config, &chosen, "Answer from note.txt.".into(), &f.grant)
        .expect("start");
    let model = ScriptedModel::new(vec![final_response()]);
    let (_tx, rx) = watch::channel(false);
    let view = f
        .advance(&mut j, &model, rx)
        .await
        .expect("checked ungrounded response");
    assert_eq!(view.phase, Phase::Failed);
    assert_eq!(view.failure.as_deref(), Some("skill_source_required"));
    assert!(view.final_text.is_none() && view.source_references.is_empty());
}

#[tokio::test]
async fn citations_require_an_actual_authorized_read_and_still_current_content() {
    let f = Fixture::new();
    let mut j = f.open();
    let chosen = pin("orange.source-question");
    j.set_skill_enabled("alice", &chosen, 0, true)
        .expect("activate");
    j.start_agent_skill(&f.config, &chosen, "Read note.txt.".into(), &f.grant)
        .expect("start");
    let model = ScriptedModel::new(vec![
        tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let intended = f.advance(&mut j, &model, rx.clone()).await.expect("intent");
    let op = intended.pending.first().expect("approval required");
    j.approve_agent_tool(
        "alice",
        "run-1",
        &op.operation_id,
        f.config.deadline_unix,
        &f.grant,
    )
    .expect("explicit owner approval");
    let mut view = f
        .advance(&mut j, &model, rx.clone())
        .await
        .expect("read executed");
    for _ in 0..3 {
        if view.phase == Phase::Completed {
            break;
        }
        view = f
            .advance(&mut j, &model, rx.clone())
            .await
            .expect("completed read");
    }
    assert_eq!(view.phase, Phase::Completed);
    assert_eq!(view.source_references.len(), 1);
    let source = &view.source_references[0];
    assert_eq!(source.path, "note.txt");
    assert_eq!(source.start_byte, "0");
    let bytes = std::fs::read(f.workspace.path().join("note.txt")).expect("fixture bytes");
    use sha2::{Digest, Sha256};
    assert_eq!(source.sha256, hex::encode(Sha256::digest(&bytes)));
    assert_eq!(source.end_byte, bytes.len().to_string());
    assert!(
        j.agent_view("alice", "run-1")
            .expect("metadata")
            .final_text
            .is_none()
    );
    drop(j);
    let j = f.open();
    assert_eq!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .expect("original content")
            .source_references,
        view.source_references
    );
    std::fs::write(f.workspace.path().join("note.txt"), "changed").expect("rotate source");
    assert!(
        j.agent_view_authorized("alice", "run-1", &f.grant)
            .await
            .is_err()
    );
}

#[test]
fn packaged_templates_match_the_offline_schema_and_catalog_pins() {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../contracts/json-schema/skill-template.v1.json"
    ))
    .expect("template schema");
    let validator = jsonschema::validator_for(&schema).expect("local schema");
    for t in crate::skills::bundled().expect("immutable pins") {
        let mut doc = serde_json::to_value(&t).expect("manifest");
        assert!(validator.is_valid(&doc));
        doc["script"] = json!("not-registered");
        assert!(!validator.is_valid(&doc));
    }
}
