//! Actual SQLite effects; scripted model transport, not real inference.
use super::*;
use crate::memory::{MemoryGrant, MemoryMutation, MemoryOrigin, MemoryPayload, MemoryWriteRequest};
fn seed(j: &mut Journal) -> MemoryGrant {
    j.create_memory_scope("alice", "personal", false)
        .expect("scope");
    j.register_run("alice", "user-seed", "policy-v1")
        .expect("user source");
    let grant = j
        .authorize_memory("alice", "personal", "policy-v1", true)
        .expect("grant");
    user_change(j, &grant, None, MemoryMutation::Put, true);
    grant
}
fn user_change(
    j: &mut Journal,
    grant: &MemoryGrant,
    version: Option<&str>,
    mutation: MemoryMutation,
    enabled: bool,
) {
    let draft = j
        .prepare_memory_write(
            grant,
            &format!("user-{}", version.unwrap_or("new")),
            MemoryWriteRequest {
                entry_id: "preference".into(),
                expected_version: version.map(str::to_owned),
                source_run: "user-seed".into(),
                origin: MemoryOrigin::UserDeclaration,
                mutation,
                payload: (mutation == MemoryMutation::Put).then(|| MemoryPayload {
                    title: "Preference".into(),
                    content: "Jasmine tea".into(),
                    enabled,
                }),
            },
        )
        .expect("user preview");
    let digest = draft.view(j).expect("view").digest.clone();
    j.approve_memory_write(draft, &digest)
        .expect("user approval");
}
fn update(version: u32, enabled: bool) -> ResponseCandidate {
    tools(vec![call(
        &format!("update-{version}"),
        "memory_write",
        json!({
            "scope_id":"personal","entry_id":"preference","expected_version":version.to_string(),
            "mutation":"put","payload":{"title":"Preference","content":format!("Tea version {}",version+1),"enabled":enabled}
        }),
    )])
}
async fn start_read(
    f: &Fixture,
    j: &mut Journal,
    auth: AgentAuthority<'_>,
    model: &ScriptedModel,
    rx: watch::Receiver<bool>,
) {
    j.read_memory_for_agent(&auth.memories[0], "preference", Some("1"), 0, 4096, false)
        .expect("seed actually readable")
        .view(j)
        .expect("live seed");
    j.start_agent_authorized(
        &f.config,
        vec![Message::plain(
            Role::User,
            "Read and update my preference after approval.".into(),
        )],
        auth,
        false,
    )
    .expect("start");
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(tools(vec![call(
            "read-original",
            "memory_read",
            json!({"scope_id":"personal","entry_id":"preference","expected_version":"1"}),
        )]));
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, model, rx.clone())
        .await
        .expect("read proposal");
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &view.pending[0].operation_id,
            preview_digest: &view.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("read approval");
    j.advance_agent_authorized("alice", "run-1", auth, model, rx)
        .await
        .expect("read effect");
    let (state, result): (String, Option<String>) = j
        .connection
        .query_row(
            "SELECT state,model_result FROM operations WHERE call_id='read-original'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("actual read outcome");
    assert_eq!(state, "succeeded", "{result:?}");
}
async fn propose_update(
    j: &mut Journal,
    auth: AgentAuthority<'_>,
    model: &ScriptedModel,
    rx: watch::Receiver<bool>,
    version: u32,
    enabled: bool,
) -> String {
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(update(version, enabled));
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, model, rx)
        .await
        .expect("update proposal");
    let op = view.pending[0].operation_id.clone();
    let preview = j
        .agent_memory_preview("alice", "run-1", &op, auth)
        .await
        .expect("source-aware preview");
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &op,
            preview_digest: &preview.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("update approval");
    op
}
#[tokio::test]
async fn own_contiguous_approved_updates_preserve_history_and_complete() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    start_read(&f, &mut j, auth, &model, rx.clone()).await;
    for version in 1..=2 {
        propose_update(&mut j, auth, &model, rx.clone(), version, true).await;
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("approved update effect");
    }
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    let done = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx)
        .await
        .expect("continue after own updates");
    assert_eq!(done.phase, Phase::Completed);
    {
        let requests = model.requests.lock().expect("requests");
        let last = requests.last().expect("last");
        assert!(
            last.messages
                .iter()
                .any(|m| m.role == Role::Tool && m.content.contains("Jasmine tea")),
            "original input remains immutable"
        );
    }
    assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        j.connection
            .query_row(
                "SELECT version FROM memory_entries WHERE entry_id='preference'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("version"),
        3
    );
    j.agent_view_authorized_scopes("alice", "run-1", auth)
        .await
        .expect("authorized final view");
}
#[tokio::test]
async fn external_replacement_disable_and_delete_block_old_context_before_model() {
    for (mutation, enabled) in [
        (MemoryMutation::Put, true),
        (MemoryMutation::Put, false),
        (MemoryMutation::Delete, false),
    ] {
        let f = Fixture::new();
        let mut j = f.open();
        let grants = vec![seed(&mut j)];
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        let model = ScriptedModel::new(vec![]);
        let (_tx, rx) = watch::channel(false);
        start_read(&f, &mut j, auth, &model, rx.clone()).await;
        user_change(&mut j, &grants[0], Some("1"), mutation, enabled);
        model
            .responses
            .lock()
            .expect("queue")
            .push_back(final_response());
        let stopped = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await;
        match stopped {
            Ok(view) => {
                assert_eq!(view.phase, Phase::RecoveryRequired);
                assert!(view.final_text.is_none());
            }
            Err(EngineError::RecoveryRequired) => {}
            Err(e) => panic!("unexpected failure: {e:?}"),
        }
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "stale body never submitted"
        );
        let view = j
            .agent_view_authorized_scopes("alice", "run-1", auth)
            .await
            .expect("content-free recovery metadata remains readable");
        assert_eq!(view.phase, Phase::RecoveryRequired);
        assert!(
            view.final_text.is_none() && view.pending.is_empty() && view.user_question.is_none()
        );
    }
}
#[tokio::test]
async fn lost_update_receipt_requires_explicit_reconciliation_without_replaying_effect() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    start_read(&f, &mut j, auth, &model, rx.clone()).await;
    let op = propose_update(&mut j, auth, &model, rx.clone(), 1, true).await;
    j.connection.execute_batch("CREATE TRIGGER lose_update_receipt BEFORE UPDATE ON operations WHEN NEW.state='succeeded' AND NEW.tool_version='memory_write.v1' BEGIN SELECT RAISE(ABORT,'owned update receipt fault');END;").expect("inject");
    assert!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .is_err()
    );
    let outcome = j
        .agent_memory_metadata("alice", "run-1", &op)
        .expect("metadata")
        .expect("outcome");
    assert_eq!(outcome.state, "approved");
    assert_eq!(outcome.result_version.as_deref(), Some("2"));
    j.connection
        .execute_batch("DROP TRIGGER lose_update_receipt;")
        .expect("remove");
    assert!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .is_err(),
        "no automatic replay"
    );
    assert!(
        f.grant.is_revoked(),
        "retry on unknown withdraws execution capability"
    );
    let fresh = WorkspaceGrant::open(
        "alice",
        "grant-v1",
        "policy-v1",
        f.workspace.path(),
        &[f.private.path().into()],
    )
    .expect("explicit fresh workspace grant");
    let auth = AgentAuthority::read(&fresh).with_memory(&grants);
    j.reconcile_agent_memory(
        "alice",
        "run-1",
        &op,
        &outcome.digest,
        MemoryRecovery::ConfirmAppliedAndContinue(auth),
    )
    .await
    .expect("explicit witnessed reconcile");
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("continue")
            .phase,
        Phase::Completed
    );
    assert_eq!(
        j.connection
            .query_row(
                "SELECT version FROM memory_entries WHERE entry_id='preference'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("one effect"),
        2
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn own_disabling_write_cannot_reexpose_previously_read_body() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    start_read(&f, &mut j, auth, &model, rx.clone()).await;
    propose_update(&mut j, auth, &model, rx.clone(), 1, false).await;
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("disabling effect");
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    let view = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx)
        .await
        .expect("recovery metadata");
    assert_eq!(view.phase, Phase::RecoveryRequired);
    assert!(view.final_text.is_none() && view.pending.is_empty());
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn own_update_keeps_transitive_file_dependency_and_source_withdrawal_blocks_reuse() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let entry = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let mut config = f.config.clone();
    config.run_id = "updater".into();
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "read-derived",
            "memory_read",
            json!({"scope_id":"personal","entry_id":entry,"expected_version":"1"}),
        )]),
        tools(vec![call(
            "update-derived",
            "memory_write",
            json!({"scope_id":"personal","entry_id":entry,"expected_version":"1","mutation":"put","payload":{"title":"Derived update","content":"Rephrased source","enabled":true}}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &config,
        vec![Message::plain(Role::User, "Update after review".into())],
        auth,
        false,
    )
    .expect("start");
    let view = j
        .advance_agent_authorized("alice", "updater", auth, &model, rx.clone())
        .await
        .expect("read proposal");
    j.approve_agent_memory(
        "alice",
        "updater",
        AgentMemoryApproval {
            operation_id: &view.pending[0].operation_id,
            preview_digest: &view.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("read approval");
    j.advance_agent_authorized("alice", "updater", auth, &model, rx.clone())
        .await
        .expect("read effect");
    let view = j
        .advance_agent_authorized("alice", "updater", auth, &model, rx.clone())
        .await
        .expect("update proposal");
    let op = &view.pending[0].operation_id;
    let preview = j
        .agent_memory_preview("alice", "updater", op, auth)
        .await
        .expect("review");
    j.approve_agent_memory(
        "alice",
        "updater",
        AgentMemoryApproval {
            operation_id: op,
            preview_digest: &preview.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approval");
    j.advance_agent_authorized("alice", "updater", auth, &model, rx.clone())
        .await
        .expect("effect");
    let select = || MemoryReadSelection {
        entry_id: entry.clone(),
        expected_version: Some("2".into()),
        start_byte: 0,
        max_bytes: 4096,
        include_disabled: false,
    };
    let sources = vec![f.grant.clone()];
    let management = MemoryManagementAuthority::new(&grants[0]).with_source_workspaces(&sources);
    let receipt = j
        .read_memory_verified(management, select())
        .await
        .expect("derived current fact remains usable");
    assert_eq!(receipt.view(&j).expect("view").content, "Rephrased source");
    drop(receipt);
    std::fs::write(f.workspace.path().join("note.txt"), "source replaced")
        .expect("external source change");
    assert!(j.read_memory_verified(management, select()).await.is_err());
    let view = j
        .advance_agent_authorized("alice", "updater", auth, &model, rx)
        .await
        .expect("recovery metadata");
    assert_eq!(view.phase, Phase::RecoveryRequired);
    assert!(view.final_text.is_none());
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        2,
        "old file input never reused for model submission"
    );
}

#[tokio::test]
async fn source_set_missing_or_extra_reference_blocks_cached_result_before_model() {
    for missing in [true, false] {
        let f = Fixture::new();
        let mut j = f.open();
        let grants = vec![seed(&mut j)];
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        let model = ScriptedModel::new(vec![]);
        let (_tx, rx) = watch::channel(false);
        start_read(&f, &mut j, auth, &model, rx.clone()).await;
        if missing {
            j.connection
                .execute_batch(
                    "DROP TRIGGER memory_read_no_delete; DELETE FROM runtime_memory_sources;",
                )
                .expect("explicit durable-corruption fixture");
        } else {
            let draft = j
                .prepare_memory_write(
                    &grants[0],
                    "second-user-entry",
                    MemoryWriteRequest {
                        entry_id: "other".into(),
                        expected_version: None,
                        source_run: "user-seed".into(),
                        origin: MemoryOrigin::UserDeclaration,
                        mutation: MemoryMutation::Put,
                        payload: Some(MemoryPayload {
                            title: "Other".into(),
                            content: "Another fact".into(),
                            enabled: true,
                        }),
                    },
                )
                .expect("new entry");
            let hash = draft.view(&j).expect("review").digest.clone();
            j.approve_memory_write(draft, &hash).expect("approve");
            j.connection.execute("INSERT INTO runtime_memory_sources SELECT operation_id,scope_id,'other',1 FROM runtime_memory_sources",[]).expect("explicit extra-reference corruption fixture");
        }
        model
            .responses
            .lock()
            .expect("queue")
            .push_back(final_response());
        let view = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("content-free recovery");
        assert_eq!(view.phase, Phase::RecoveryRequired);
        assert!(view.final_text.is_none());
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn source_set_migration_preserves_actual_v25_read_and_immutable_bindings() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    start_read(&f, &mut j, auth, &model, rx.clone()).await;
    let original: String = j
        .connection
        .query_row(
            "SELECT model_result FROM operations WHERE call_id='read-original'",
            [],
            |r| r.get(0),
        )
        .expect("immutable payload");
    // Reconstruct precisely the previous table layout, retaining real effects
    // and snapshots. Actual old-binary compatibility is additionally checked by
    // scripts/check-file-editing.py; this fixture specifically carries memory.
    j.connection.execute_batch("DROP VIEW available_skill_versions; DROP TABLE local_run_skills; DROP TABLE local_skill_selections; DROP TABLE local_skill_packages; DROP TABLE runtime_skill_bindings; DROP TABLE owner_skills; DROP TABLE skill_versions; DROP TABLE memory_owner_removals; DROP TABLE runtime_memory_continuity_policy; DROP INDEX memory_approved_version; DROP TABLE runtime_memory_protocol; DROP TRIGGER memory_read_source_scope; DROP TRIGGER memory_read_source_bound; DROP TRIGGER memory_read_binding; DROP TRIGGER memory_read_no_delete; ALTER TABLE runtime_memory_sources RENAME TO source_fixture;").expect("prior layout fixture");
    let legacy = include_str!("../../schema_v23.sql");
    for line in legacy.lines().filter(|line| {
        line.starts_with("CREATE TABLE runtime_memory_sources")
            || line.starts_with("CREATE TRIGGER memory_read_")
    }) {
        j.connection.execute_batch(line).expect("exact prior DDL");
    }
    j.connection.execute_batch("INSERT INTO runtime_memory_sources SELECT * FROM source_fixture; DROP TABLE source_fixture; PRAGMA user_version=25;").expect("retain v25 references");
    drop(j);
    let mut j = f.open();
    assert_eq!(
        j.connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .expect("schema"),
        31
    );
    assert_eq!(
        j.connection
            .query_row(
                "SELECT model_result FROM operations WHERE call_id='read-original'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("immutable payload"),
        original
    );
    assert!(
        j.connection
            .execute("UPDATE runtime_memory_sources SET entry_version=2", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM runtime_memory_sources", [])
            .is_err()
    );
    j.create_memory_scope("alice", "another-scope", false)
        .expect("other scope");
    assert!(j.connection.execute("INSERT INTO runtime_memory_sources SELECT operation_id,'another-scope',entry_id,entry_version FROM runtime_memory_sources",[]).is_err(),"operation scope cannot expand");
    let fresh = vec![
        j.authorize_memory("alice", "personal", "policy-v1", true)
            .expect("fresh explicit memory grant"),
    ];
    let auth = AgentAuthority::read(&f.grant).with_memory(&fresh);
    model
        .responses
        .lock()
        .expect("queue")
        .push_back(final_response());
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("old committed read continues")
            .phase,
        Phase::Completed
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        j.connection
            .query_row(
                "SELECT count(*) FROM operations WHERE call_id='read-original'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("no read replay"),
        1
    );
}

#[tokio::test]
async fn source_set_database_bound_is_one_hundred_and_cannot_change_scope() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![]);
    let (_tx, rx) = watch::channel(false);
    start_read(&f, &mut j, auth, &model, rx).await;
    // Deliberate extra-reference corruption: test only the database bound, not
    // valid discovery results. Runtime separately rejects this v1 result set.
    // Reuse an existing FK entry with distinct owned fixture scopes and prove
    // scope binding is enforced before exercising the bound in the real scope.
    let op: String = j
        .connection
        .query_row("SELECT operation_id FROM runtime_memory_sources", [], |r| {
            r.get(0)
        })
        .expect("operation");
    j.create_memory_scope("alice", "foreign", false)
        .expect("other scope");
    assert!(
        j.connection
            .execute(
                "INSERT INTO runtime_memory_sources VALUES(?,'foreign','preference',1)",
                [&op]
            )
            .is_err()
    );
    for n in 1..=100 {
        let id = format!("bounded-{n}");
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
                        title: "Owned fixture".into(),
                        content: "Bound check".into(),
                        enabled: true,
                    }),
                },
            )
            .expect("actual entry");
        let hash = draft.view(&j).expect("review").digest.clone();
        j.approve_memory_write(draft, &hash).expect("approve");
        let inserted = j.connection.execute(
            "INSERT INTO runtime_memory_sources VALUES(?,'personal',?,1)",
            params![op, id],
        );
        if n < 100 {
            assert_eq!(inserted.expect("within table bound"), 1);
        } else {
            assert!(inserted.is_err(), "101st reference must fail");
        }
    }
    assert_eq!(
        j.connection
            .query_row(
                "SELECT count(*) FROM runtime_memory_sources WHERE operation_id=?",
                [&op],
                |r| r.get::<_, i64>(0)
            )
            .expect("bounded total"),
        100
    );
    assert!(
        j.checked_memory_source_rows(&f.config, &j.operation_record(&f.config, &op).expect("op"))
            .is_err(),
        "multi-row table cannot upgrade an old single-entry tool result"
    );
}
