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
    for name in ["orange.source-question", "orange.code-review"] {
        let mut f = Fixture::new();
        if name == "orange.code-review" {
            f.config.model_version = "qwen7b-reviewed-cpu-v1".into();
        }
        let mut j = f.open();
        let chosen = pin(name);
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
}

#[tokio::test]
async fn a_file_answer_without_committed_source_is_not_published() {
    for name in ["orange.source-question", "orange.code-review"] {
        let mut f = Fixture::new();
        if name == "orange.code-review" {
            f.config.model_version = "qwen7b-reviewed-cpu-v1".into();
        }
        let mut j = f.open();
        let chosen = pin(name);
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

fn package(version: &str, instruction: &str) -> Vec<u8> {
    serde_json::json!({
        "id":"local.release-notes", "version":version, "scenario":"AU-02",
        "name":"Release notes", "instruction":instruction, "tools":[]
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn local_package_review_install_remove_and_recover_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let f = Fixture::new();
    let mut j = f.open();
    let bytes = package("1.0.0", "Summarize the supplied changelog.");
    let review = j.review_skill_package("alice", &bytes)?;
    let pin = review.pin;
    assert_eq!(
        (
            review.installation_revision,
            review.activation_revision,
            review.installed
        ),
        (0, 0, false)
    );
    assert!(
        j.install_skill_package("alice", &bytes, &"0".repeat(64), 0)
            .is_err()
    );
    for _ in 0..2 {
        assert_eq!(j.install_skill_package("alice", &bytes, &pin.digest, 0)?, 1);
    }
    assert!(
        j.start_agent_skill(&f.config, &pin, "input".into(), &f.grant)
            .is_err()
    );
    j.set_skill_enabled("alice", &pin, 0, true)?;
    j.start_agent_skill(&f.config, &pin, "input".into(), &f.grant)?;
    assert!(j.uninstall_skill_package("alice", &pin, 1, 0).is_err());
    assert_eq!(j.uninstall_skill_package("alice", &pin, 1, 1)?, 2);
    assert_eq!(j.uninstall_skill_package("alice", &pin, 1, 1)?, 2);
    drop(j);
    let mut j = f.open();
    assert!(j.skills("alice")?.iter().all(|s| s.pin != pin));
    assert!(
        j.install_skill_package("alice", &bytes, &pin.digest, 0)
            .is_err()
    );
    let other = package("1.0.0", "Different reviewed content");
    assert!(j.review_skill_package("alice", &other).is_err());
    let bob = j.review_skill_package("bob", &other)?.pin;
    assert_ne!(pin, bob);
    assert!(j.set_skill_enabled("bob", &pin, 0, true).is_err());
    j.install_skill_package("bob", &other, &bob.digest, 0)?;
    j.set_skill_enabled("bob", &bob, 0, true)?;
    assert!(j.run_skill("bob", "run-1").is_err());
    assert!(j.uninstall_skill_package("bob", &pin, 1, 1).is_err());
    j.install_skill_package("alice", &bytes, &pin.digest, 2)?;
    j.set_skill_enabled("alice", &pin, 2, true)?;
    assert!(
        j.start_agent_skill_reviewed(&f.config, &pin, Some(1), "input".into(), &f.grant)
            .is_err()
    );
    assert!(
        j.start_agent_skill_reviewed(&f.config, &pin, Some(3), "input".into(), &f.grant)
            .is_err()
    );
    let mut fresh = f.config.clone();
    fresh.run_id = "fresh-run".into();
    assert!(
        j.start_agent_skill_reviewed(&fresh, &pin, Some(1), "input".into(), &f.grant)
            .is_err()
    );
    j.start_agent_skill_reviewed(&fresh, &pin, Some(3), "input".into(), &f.grant)
        .expect("fresh run with reviewed revision");
    assert_eq!(j.run_skill("alice", "run-1")?, Some(pin));
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    let view = f.advance(&mut j, &model, rx).await?;
    assert_eq!(view.phase, Phase::Cancelled);
    assert!(
        model
            .requests
            .lock()
            .expect("recorded model requests")
            .is_empty()
    );
    assert_eq!(
        j.skills("bob")?
            .iter()
            .filter(|s| s.enabled)
            .map(|s| &s.pin)
            .collect::<Vec<_>>(),
        vec![&bob]
    );
    Ok(())
}

#[test]
fn local_version_switch_requires_current_selection_and_drops_unused_document()
-> Result<(), Box<dyn std::error::Error>> {
    let f = Fixture::new();
    let mut j = f.open();
    let first = package("1.0.0", "Answer concisely.");
    let second = package("1.1.0", "Return two sentences.");
    let a = j.review_skill_package("alice", &first)?.pin;
    let b = j.review_skill_package("alice", &second)?.pin;
    j.install_skill_package("alice", &first, &a.digest, 0)?;
    j.install_skill_package("alice", &second, &b.digest, 0)?;
    assert_eq!(j.set_skill_enabled("alice", &a, 0, true)?, 1);
    assert_eq!(j.set_skill_enabled("alice", &b, 1, true)?, 2);
    assert!(j.set_skill_enabled("alice", &a, 1, true).is_err());
    assert_eq!(j.set_skill_enabled("alice", &a, 2, false)?, 2);
    assert_eq!(j.set_skill_enabled("alice", &a, 2, true)?, 3);
    assert_eq!(j.uninstall_skill_package("alice", &b, 1, 3)?, 2);
    let document: Option<String> = j.connection.query_row(
        "SELECT document FROM local_skill_packages WHERE digest=?",
        [&b.digest],
        |r| r.get(0),
    )?;
    assert!(document.is_none());
    assert_eq!(
        j.skills("alice")?
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.pin)
            .collect::<Vec<_>>(),
        vec![a]
    );
    Ok(())
}

#[test]
fn local_review_rejects_unknown_fields_namespace_and_tool_escalation()
-> Result<(), Box<dyn std::error::Error>> {
    let bytes = package("1.0.0", "Only use the source.");
    assert!(
        crate::skills::review_local_skill(&vec![b' '; crate::skills::LOCAL_SKILL_MAX_BYTES + 1])
            .is_err()
    );
    for (field, value) in [
        ("id", serde_json::json!("orange.text-summary")),
        ("code", serde_json::json!("noop")),
        ("version", serde_json::json!("01.0.0")),
        ("tools", serde_json::json!(["write_file"])),
        ("tools", serde_json::json!(["read_file", "read_file"])),
    ] {
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes)?;
        doc[field] = value;
        assert!(crate::skills::review_local_skill(&serde_json::to_vec(&doc)?).is_err());
    }
    Ok(())
}

#[test]
fn official_review_refuses_an_unqualified_model_before_creating_a_run()
-> Result<(), Box<dyn std::error::Error>> {
    let f = Fixture::new();
    let mut j = f.open();
    let p = pin("orange.code-review");
    j.set_skill_enabled("alice", &p, 0, true)?;
    let declared = j
        .skills("alice")?
        .into_iter()
        .find(|row| row.pin == p)
        .expect("official template");
    assert_eq!(
        declared.required_model_profile,
        Some("qwen7b-reviewed-cpu-v1")
    );
    assert!(matches!(
        j.start_agent_skill(&f.config, &p, "Review note.txt".into(), &f.grant),
        Err(EngineError::Forbidden)
    ));
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM runtime_runs", [], |r| r.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}
