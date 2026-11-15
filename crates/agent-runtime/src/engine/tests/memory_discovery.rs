//! Real SQLite and file state with explicitly scripted model responses.
use super::*;
use crate::{
    catalog::ToolCatalog,
    memory::{
        MemoryGrant, MemoryMutation, MemoryOrigin, MemoryPayload, MemoryWriteRequest,
        tools::MemoryRequest,
    },
};
fn setup(j: &mut Journal) -> Vec<MemoryGrant> {
    j.create_memory_scope("alice", "personal", false)
        .expect("scope");
    j.register_run("alice", "user-seed", "policy-v1")
        .expect("seed run");
    let grant = j
        .authorize_memory("alice", "personal", "policy-v1", true)
        .expect("grant");
    for id in ["a", "b"] {
        let draft = j
            .prepare_memory_write(
                &grant,
                id,
                MemoryWriteRequest {
                    entry_id: id.into(),
                    expected_version: None,
                    source_run: "user-seed".into(),
                    origin: MemoryOrigin::UserDeclaration,
                    mutation: MemoryMutation::Put,
                    payload: Some(MemoryPayload {
                        title: format!("Title {id}"),
                        content: format!("Content {id}"),
                        enabled: true,
                    }),
                },
            )
            .expect("prepare");
        let hash = draft.view(j).expect("view").digest.clone();
        j.approve_memory_write(draft, &hash).expect("approve");
    }
    vec![grant]
}
fn prompt() -> Vec<Message> {
    vec![Message::plain(
        Role::User,
        "Discover and read my approved memories.".into(),
    )]
}
fn push(model: &ScriptedModel, id: &str, args: serde_json::Value) {
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(tools(vec![call(id, "memory_read", args)]));
}
async fn execute(
    j: &mut Journal,
    run: &str,
    auth: AgentAuthority<'_>,
    model: &ScriptedModel,
    rx: watch::Receiver<bool>,
) -> String {
    let view = j
        .advance_agent_authorized("alice", run, auth, model, rx.clone())
        .await
        .expect("proposal");
    assert_eq!(view.phase, Phase::ToolsPending);
    let op = &view.pending[0];
    j.approve_agent_memory(
        "alice",
        run,
        AgentMemoryApproval {
            operation_id: &op.operation_id,
            preview_digest: &op.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("explicit approval");
    j.advance_agent_authorized("alice", run, auth, model, rx)
        .await
        .expect("actual execution");
    op.operation_id.clone()
}
fn result(j: &Journal, id: &str) -> serde_json::Value {
    let raw: String = j
        .connection
        .query_row(
            "SELECT model_result FROM operations WHERE operation_id=?",
            [id],
            |r| r.get(0),
        )
        .expect("committed result");
    serde_json::from_str(&raw).expect("json")
}
#[tokio::test]
async fn explicit_v2_pages_then_entry_and_derived_write_retain_every_source() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = setup(&mut j);
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    j.start_agent_discovering(&f.config, prompt(), auth, false)
        .expect("opt-in");
    assert_eq!(
        j.agent_memory_read_version("alice", "run-1")
            .expect("protocol"),
        2
    );
    assert!(
        j.start_agent_authorized(&f.config, prompt(), auth, false)
            .is_err(),
        "same Run cannot change protocol"
    );
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    push(
        &model,
        "page1",
        json!({"mode":"list","scope_id":"personal","limit":1}),
    );
    let first = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    let page = result(&j, &first);
    let cursor = page["data"]["page"]["next_cursor"]
        .as_str()
        .expect("cursor");
    assert_eq!(page["data"]["page"]["entries"][0]["entry_id"], "a");
    assert!(!page.to_string().contains("Content a"));
    push(
        &model,
        "page2",
        json!({"mode":"list","scope_id":"personal","limit":1,"cursor":cursor}),
    );
    let second = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    assert_eq!(
        result(&j, &second)["data"]["page"]["entries"][0]["entry_id"],
        "b"
    );
    push(
        &model,
        "entry",
        json!({"mode":"entry","scope_id":"personal","entry_id":"a","expected_version":"1"}),
    );
    let read = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    assert_eq!(result(&j, &read)["data"]["value"]["content"], "Content a");
    model.responses.lock().expect("queue").push_back(tools(vec![call("write","memory_write",json!({"scope_id":"personal","mutation":"put","payload":{"title":"Combined","content":"Derived from discovered notes","enabled":true}}))]));
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("write proposal");
    let op = &view.pending[0].operation_id;
    let preview = j
        .agent_memory_preview("alice", "run-1", op, auth)
        .await
        .expect("source preview");
    assert_eq!(
        preview.provenance.sources.len(),
        2,
        "both page summaries contribute, entry read is deduplicated"
    );
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: op,
            preview_digest: &preview.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve write");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("write effect");
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("finish")
            .phase,
        Phase::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 5);
}
#[tokio::test]
async fn listed_entry_deletion_and_reference_corruption_block_next_model() {
    for corrupt in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        let grants = setup(&mut j);
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        j.start_agent_discovering(&f.config, prompt(), auth, false)
            .expect("start");
        let model = ScriptedModel::new(vec![]);
        let (_tx, rx) = watch::channel(false);
        push(&model, "list", json!({"mode":"list","scope_id":"personal"}));
        let op = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
        assert_eq!(
            j.connection
                .query_row(
                    "SELECT count(*) FROM runtime_memory_sources WHERE operation_id=?",
                    [&op],
                    |r| r.get::<_, i64>(0)
                )
                .expect("sources"),
            2
        );
        if corrupt {
            j.connection.execute_batch("DROP TRIGGER memory_read_no_delete; DELETE FROM runtime_memory_sources WHERE entry_id='b';").expect("explicit corruption fixture");
        } else {
            let draft = j
                .prepare_memory_write(
                    &grants[0],
                    "delete",
                    MemoryWriteRequest {
                        entry_id: "b".into(),
                        expected_version: Some("1".into()),
                        source_run: "user-seed".into(),
                        origin: MemoryOrigin::UserDeclaration,
                        mutation: MemoryMutation::Delete,
                        payload: None,
                    },
                )
                .expect("delete preview");
            let hash = draft.view(&j).expect("review").digest.clone();
            j.approve_memory_write(draft, &hash).expect("delete");
        }
        model
            .responses
            .lock()
            .expect("queue")
            .push_back(final_response());
        assert_eq!(
            j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
                .await
                .expect("metadata")
                .phase,
            Phase::RecoveryRequired
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn protocol_and_cursor_survive_only_their_intended_boundaries() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = setup(&mut j);
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    j.start_agent_discovering(&f.config, prompt(), auth, false)
        .expect("start");
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    push(
        &model,
        "list",
        json!({"mode":"list","scope_id":"personal","limit":1}),
    );
    let op = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    let cursor = result(&j, &op)["data"]["page"]["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    assert!(
        j.connection
            .execute("UPDATE runtime_memory_protocol SET read_version=1", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM runtime_memory_protocol", [])
            .is_err()
    );
    let mut legacy = f.config.clone();
    legacy.run_id = "legacy".into();
    j.start_agent_authorized(&legacy, prompt(), auth, false)
        .expect("v1 remains opt-out");
    drop(j);
    let mut j = f.open();
    let grants = vec![
        j.authorize_memory("alice", "personal", "policy-v1", true)
            .expect("fresh"),
    ];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    assert_eq!(
        j.agent_memory_read_version("alice", "run-1")
            .expect("saved"),
        2
    );
    assert_eq!(
        j.agent_memory_read_version("alice", "legacy")
            .expect("legacy"),
        1
    );
    push(
        &model,
        "stale-page",
        json!({"mode":"list","scope_id":"personal","cursor":cursor}),
    );
    let failed = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    assert_eq!(result(&j, &failed), json!({"error":"memory_unavailable"}));
    push(
        &model,
        "new-page",
        json!({"mode":"list","scope_id":"personal"}),
    );
    let good = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    assert_eq!(
        result(&j, &good)["data"]["page"]["entries"]
            .as_array()
            .expect("entries")
            .len(),
        2
    );
    push(
        &model,
        "old-read",
        json!({"scope_id":"personal","entry_id":"a"}),
    );
    let old = execute(&mut j, "legacy", auth, &model, rx).await;
    assert_eq!(result(&j, &old)["data"]["content"], "Content a");
}
#[test]
fn v2_schema_is_explicit_bounded_and_cannot_change_v1_hashes() {
    let old = ToolCatalog::new(crate::memory::tools::specs(true)).expect("v1");
    let new = ToolCatalog::new(crate::memory::tools::discovery_specs(true)).expect("v2");
    assert_eq!(
        old.pin("memory_write").expect("old"),
        new.pin("memory_write").expect("new")
    );
    assert_ne!(
        old.pin("memory_read").expect("old"),
        new.pin("memory_read").expect("new")
    );
    let pin = new.pin("memory_read").expect("pin");
    for args in [
        json!({"mode":"list","scope_id":"personal","limit":100}),
        json!({"mode":"entry","scope_id":"personal","entry_id":"a"}),
    ] {
        new.validate_input(&pin, &args).expect("valid schema");
        MemoryRequest::parse("memory_read", &args).expect("valid parse");
    }
    for args in [
        json!({"scope_id":"personal","entry_id":"a"}),
        json!({"mode":"list","scope_id":"personal","entry_id":"a"}),
        json!({"mode":"list","scope_id":"personal","limit":101}),
        json!({"mode":"entry","scope_id":"personal","entry_id":"a","cursor":"a"}),
        json!({"mode":"list","scope_id":"personal","actor":"bob"}),
        json!({"mode":"list","scope_id":"personal","include_disabled":true}),
    ] {
        assert!(new.validate_input(&pin, &args).is_err());
        if args.get("mode").is_some() {
            assert!(MemoryRequest::parse("memory_read", &args).is_err());
        }
    }
    let bindings = vec![crate::memory::MemoryCapability {
        scope_id: "personal".into(),
        revision: "1".into(),
        write: true,
        environment: "scope".into(),
    }];
    let scoped = ToolCatalog::builtin_memory_discovery_scoped(false, false, false, &bindings)
        .expect("scoped");
    assert!(
        scoped
            .validate_input(
                &scoped.pin("memory_read").expect("pin"),
                &json!({"mode":"list","scope_id":"foreign"})
            )
            .is_err()
    );
}

#[tokio::test]
async fn source_only_capabilities_do_not_expand_discovery_and_withdrawal_stops_old_pages() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = setup(&mut j);
    let derived = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    j.create_memory_scope("alice", "source-only-secret", false)
        .expect("source scope");
    let proof_memories = vec![
        j.authorize_memory("alice", "source-only-secret", "policy-v1", false)
            .expect("proof only"),
    ];
    let other = tempfile::tempdir().expect("different root");
    let read = WorkspaceGrant::open(
        "alice",
        "other",
        "policy-v1",
        other.path(),
        &[f.private.path().into()],
    )
    .expect("primary");
    let mut config = f.config.clone();
    config.run_id = "reader".into();
    config.environment = read.environment().into();
    let auth = AgentAuthority::read(&read)
        .with_memory(&grants)
        .with_source_memories(&proof_memories);
    j.start_agent_discovering(&config, prompt(), auth, false)
        .expect("start");
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    push(
        &model,
        "filtered",
        json!({"mode":"list","scope_id":"personal"}),
    );
    let first = execute(&mut j, "reader", auth, &model, rx.clone()).await;
    let result1 = result(&j, &first);
    assert_eq!(
        result1["data"]["page"]["entries"]
            .as_array()
            .expect("entries")
            .len(),
        2
    );
    assert_eq!(result1["data"]["page"]["omitted_unavailable"], true);
    assert!(!result1.to_string().contains(&derived));
    {
        let requests = model.requests.lock().expect("requests");
        let schema = &requests[0]
            .tools
            .iter()
            .find(|v| v.name == "memory_read")
            .expect("definition")
            .parameters;
        assert!(!schema.to_string().contains("source-only-secret"));
    }
    let sources = vec![f.grant.clone()];
    let auth = auth.with_source_workspaces(&sources);
    push(
        &model,
        "sources-authorized",
        json!({"mode":"list","scope_id":"personal"}),
    );
    let second = execute(&mut j, "reader", auth, &model, rx.clone()).await;
    assert_eq!(
        result(&j, &second)["data"]["page"]["entries"]
            .as_array()
            .expect("entries")
            .len(),
        3
    );
    f.grant.request_revoke();
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    assert_eq!(
        j.advance_agent_authorized("alice", "reader", auth, &model, rx)
            .await
            .expect("metadata")
            .phase,
        Phase::RecoveryRequired
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn maximum_page_is_bounded_and_persists_one_reference_per_summary() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = setup(&mut j);
    for n in 0..99 {
        let id = format!("item-{n:03}");
        let draft = j
            .prepare_memory_write(
                &grants[0],
                &id,
                MemoryWriteRequest {
                    entry_id: id.clone(),
                    expected_version: None,
                    source_run: "user-seed".into(),
                    origin: MemoryOrigin::UserDeclaration,
                    mutation: MemoryMutation::Put,
                    payload: Some(MemoryPayload {
                        title: format!("Title {id}"),
                        content: "Owned entry".into(),
                        enabled: true,
                    }),
                },
            )
            .expect("draft");
        let hash = draft.view(&j).expect("view").digest.clone();
        j.approve_memory_write(draft, &hash).expect("approve");
    }
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    j.start_agent_discovering(&f.config, prompt(), auth, false)
        .expect("start");
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    push(
        &model,
        "max-page",
        json!({"mode":"list","scope_id":"personal","limit":100}),
    );
    let op = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    let value = result(&j, &op);
    assert_eq!(
        value["data"]["page"]["entries"]
            .as_array()
            .expect("entries")
            .len(),
        100
    );
    assert_eq!(
        j.connection
            .query_row(
                "SELECT count(*) FROM runtime_memory_sources WHERE operation_id=?",
                [&op],
                |r| r.get::<_, i64>(0)
            )
            .expect("references"),
        100
    );
    let cursor = value["data"]["page"]["next_cursor"]
        .as_str()
        .expect("cursor");
    push(
        &model,
        "last-page",
        json!({"mode":"list","scope_id":"personal","cursor":cursor,"limit":100}),
    );
    let op = execute(&mut j, "run-1", auth, &model, rx).await;
    let last = result(&j, &op);
    assert_eq!(
        last["data"]["page"]["entries"]
            .as_array()
            .expect("last")
            .len(),
        1
    );
    assert!(last["data"]["page"]["next_cursor"].is_null());
}
#[tokio::test]
async fn expired_cursor_and_empty_page_are_bounded_results_without_invented_references() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = setup(&mut j);
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    j.start_agent_discovering(&f.config, prompt(), auth, false)
        .expect("start");
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    push(
        &model,
        "first",
        json!({"mode":"list","scope_id":"personal","limit":1}),
    );
    let op = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    let token = result(&j, &op)["data"]["page"]["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    j.memory_cursors
        .borrow_mut()
        .get_mut(&token)
        .expect("session cursor")
        .expires = std::time::Instant::now();
    push(
        &model,
        "expired",
        json!({"mode":"list","scope_id":"personal","cursor":token}),
    );
    let op = execute(&mut j, "run-1", auth, &model, rx.clone()).await;
    assert_eq!(result(&j, &op), json!({"error":"memory_unavailable"}));
    assert_eq!(
        j.connection
            .query_row(
                "SELECT count(*) FROM runtime_memory_sources WHERE operation_id=?",
                [&op],
                |r| r.get::<_, i64>(0)
            )
            .expect("no refs"),
        0
    );
    j.create_memory_scope("alice", "empty", false)
        .expect("empty scope");
    let empty = vec![
        j.authorize_memory("alice", "empty", "policy-v1", false)
            .expect("read only"),
    ];
    let auth = AgentAuthority::read(&f.grant).with_memory(&empty);
    let mut c = f.config.clone();
    c.run_id = "empty-reader".into();
    j.start_agent_discovering(&c, prompt(), auth, false)
        .expect("start empty");
    push(&model, "empty", json!({"mode":"list","scope_id":"empty"}));
    let op = execute(&mut j, "empty-reader", auth, &model, rx.clone()).await;
    assert_eq!(result(&j, &op)["data"]["page"]["entries"], json!([]));
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    assert_eq!(
        j.advance_agent_authorized("alice", "empty-reader", auth, &model, rx)
            .await
            .expect("empty context is valid")
            .phase,
        Phase::Completed
    );
}
#[test]
fn discovery_execution_pins_match_fixed_fixture_and_preserve_other_tools() {
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../contracts/fixtures/memory-tool-pins.v2.json"
    ))
    .expect("fixture");
    let all = ToolCatalog::builtin_memory_discovery(true, true, true, true).expect("catalog");
    assert_eq!(
        all.document().tools.len(),
        expected["pins"].as_object().expect("pins").len()
    );
    for spec in all.document().tools {
        assert_eq!(
            expected["pins"][&spec.tool_version].as_str(),
            Some(spec.execution_digest().expect("digest").as_str())
        );
    }
    let old = ToolCatalog::builtin_memory(true, true, true, true).expect("old");
    for spec in old
        .document()
        .tools
        .iter()
        .filter(|s| s.name != "memory_read")
    {
        assert_eq!(
            old.pin(&spec.name).expect("old"),
            all.pin(&spec.name).expect("new")
        );
    }
}

#[tokio::test]
async fn late_approval_or_scope_loss_rolls_back_all_discovery_output() {
    for approval in [true, false] {
        let f = Fixture::new();
        let mut j = f.open();
        let grants = setup(&mut j);
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        j.start_agent_discovering(&f.config, prompt(), auth, false)
            .expect("start");
        let model = ScriptedModel::new(vec![]);
        let (_tx, rx) = watch::channel(false);
        push(&model, "list", json!({"mode":"list","scope_id":"personal"}));
        let view = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("proposal");
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
        .expect("approve");
        if approval {
            j.connection.execute_batch("CREATE TRIGGER late_discovery_loss AFTER INSERT ON runtime_memory_sources BEGIN DELETE FROM approvals; END;").expect("owned approval race fixture");
        } else {
            j.connection.execute_batch("CREATE TRIGGER late_discovery_loss AFTER INSERT ON runtime_memory_sources BEGIN UPDATE memory_scopes SET enabled=0; END;").expect("owned scope race fixture");
        }
        assert!(
            j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
                .await
                .is_err()
        );
        assert_eq!(
            j.connection
                .query_row(
                    "SELECT count(*) FROM runtime_memory_sources WHERE operation_id=?",
                    [&op.operation_id],
                    |r| r.get::<_, i64>(0)
                )
                .expect("refs rolled back"),
            0
        );
        assert!(
            j.connection
                .query_row(
                    "SELECT model_result FROM operations WHERE operation_id=?",
                    [&op.operation_id],
                    |r| r.get::<_, Option<String>>(0)
                )
                .expect("no result")
                .is_none()
        );
        j.connection
            .execute_batch("DROP TRIGGER late_discovery_loss;")
            .expect("remove fault");
        assert!(
            j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
                .await
                .is_err(),
            "interrupted read never silently publishes a replacement result"
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}
