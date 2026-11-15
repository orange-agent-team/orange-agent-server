use super::*;
use crate::memory::MemoryGrant;
fn memory(j: &mut Journal) -> MemoryGrant {
    j.create_memory_scope("alice", "personal", false)
        .expect("scope");
    j.authorize_memory("alice", "personal", "policy-v1", true)
        .expect("memory grant")
}
fn write(content: &str) -> ResponseCandidate {
    tools(vec![call(
        "learn",
        "memory_write",
        json!({"scope_id":"personal","mutation":"put","payload":{"title":"Learned note","content":content,"enabled":true}}),
    )])
}
pub(super) async fn learn_file(
    j: &mut Journal,
    config: &RunConfig,
    read: &WorkspaceGrant,
    grants: &[MemoryGrant],
) -> String {
    let auth = AgentAuthority::read(read).with_memory(grants);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "source",
            "read_file",
            json!({"path":"note.txt"}),
        )]),
        write("allowed data"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        config,
        vec![Message::plain(
            Role::User,
            "Read the note and propose a memory.".into(),
        )],
        auth,
        false,
    )
    .expect("start");
    let view = j
        .advance_agent_authorized("alice", &config.run_id, auth, &model, rx.clone())
        .await
        .expect("file intent");
    j.approve_agent_tool(
        "alice",
        &config.run_id,
        &view.pending[0].operation_id,
        now().expect("time") + 60,
        read,
    )
    .expect("approve file");
    j.advance_agent_authorized("alice", &config.run_id, auth, &model, rx.clone())
        .await
        .expect("actual file read");
    let view = j
        .advance_agent_authorized("alice", &config.run_id, auth, &model, rx.clone())
        .await
        .expect("memory proposal");
    let id = &view.pending[0].operation_id;
    let preview = j
        .agent_memory_preview("alice", &config.run_id, id, auth)
        .await
        .expect("file-derived preview now supported");
    assert!(matches!(
        preview.provenance.sources.as_slice(),
        [MemorySource::Workspace { .. }]
    ));
    assert_ne!(preview.approval_digest, preview.review.digest);
    assert!(
        j.approve_agent_memory(
            "alice",
            &config.run_id,
            AgentMemoryApproval {
                operation_id: id,
                preview_digest: &preview.review.digest,
                expires_at: now().expect("time") + 60
            },
            auth
        )
        .await
        .is_err(),
        "payload-only approval cannot authorize dependencies"
    );
    j.approve_agent_memory(
        "alice",
        &config.run_id,
        AgentMemoryApproval {
            operation_id: id,
            preview_digest: &preview.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("composite source approval");
    j.advance_agent_authorized("alice", &config.run_id, auth, &model, rx.clone())
        .await
        .expect("learn actual fact");
    let done = j
        .advance_agent_authorized("alice", &config.run_id, auth, &model, rx)
        .await
        .expect("finish");
    assert_eq!(done.phase, Phase::Completed);
    preview.review.entry_id
}
#[tokio::test]
async fn file_derived_memory_requires_original_identity_current_consent_and_fresh_source_bytes() {
    let mut f = Fixture::new();
    let mut j = f.open();
    let workspace = j
        .authorize_workspace("alice", "source-consent", f.workspace.path(), "policy-v1")
        .expect("consent");
    f.grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("actual grant");
    f.config.environment = f.grant.environment().into();
    let grants = vec![memory(&mut j)];
    let entry = learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let other = tempfile::tempdir().expect("other root");
    let other_grant = WorkspaceGrant::open(
        "alice",
        "other",
        "policy-v1",
        other.path(),
        &[f.private.path().into()],
    )
    .expect("other grant");
    let mut c = f.config.clone();
    c.run_id = "reader".into();
    c.environment = other_grant.environment().into();
    let read_memories = vec![
        j.authorize_memory("alice", "personal", "policy-v1", false)
            .expect("memory read"),
    ];
    let base = AgentAuthority::read(&other_grant).with_memory(&read_memories);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "retrieve",
            "memory_read",
            json!({"scope_id":"personal","entry_id":entry}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(Role::User, "Read the saved note.".into())],
        base,
        false,
    )
    .expect("new run");
    let view = j
        .advance_agent_authorized("alice", "reader", base, &model, rx.clone())
        .await
        .expect("intent");
    let op = &view.pending[0];
    j.approve_agent_memory(
        "alice",
        "reader",
        AgentMemoryApproval {
            operation_id: &op.operation_id,
            preview_digest: &op.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        base,
    )
    .await
    .expect("approve memory read");
    assert!(
        j.advance_agent_authorized("alice", "reader", base, &model, rx.clone())
            .await
            .is_err(),
        "memory scope is not a source workspace grant"
    );
    assert_eq!(
        j.state("alice", &op.operation_id)
            .expect("no dispatch")
            .as_deref(),
        Some("intended")
    );
    let impostor = tempfile::tempdir().expect("same bytes wrong directory");
    std::fs::write(impostor.path().join("note.txt"), "allowed data").expect("bytes");
    let fake = vec![
        WorkspaceGrant::open(
            "alice",
            "fake",
            "policy-v1",
            impostor.path(),
            &[f.private.path().into()],
        )
        .expect("fake grant"),
    ];
    assert!(
        j.advance_agent_authorized(
            "alice",
            "reader",
            base.with_source_workspaces(&fake),
            &model,
            rx.clone()
        )
        .await
        .is_err()
    );
    let sources = vec![f.grant.clone()];
    let auth = base.with_source_workspaces(&sources);
    j.advance_agent_authorized("alice", "reader", auth, &model, rx.clone())
        .await
        .expect("actual separately authorized source");
    let done = j
        .advance_agent_authorized("alice", "reader", auth, &model, rx.clone())
        .await
        .expect("complete");
    assert_eq!(done.phase, Phase::Completed);
    assert!(
        j.agent_view_with_memory("alice", "reader", &other_grant, &read_memories)
            .await
            .is_err()
    );
    assert!(
        j.agent_view_authorized_scopes("alice", "reader", auth)
            .await
            .expect("authorized view")
            .final_text
            .is_some()
    );
    std::fs::write(f.workspace.path().join("note.txt"), "changed source").expect("external change");
    assert!(
        j.agent_view_authorized_scopes("alice", "reader", auth)
            .await
            .is_err()
    );
    assert!(
        j.advance_agent_authorized("alice", "reader", auth, &model, rx)
            .await
            .is_err()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    std::fs::write(f.workspace.path().join("note.txt"), "allowed data")
        .expect("owned source restore");
    drop(j);
    let mut j = f.open();
    let fresh = vec![
        j.authorize_memory("alice", "personal", "policy-v1", false)
            .expect("fresh memory permission"),
    ];
    let auth = AgentAuthority::read(&other_grant)
        .with_memory(&fresh)
        .with_source_workspaces(&sources);
    assert!(
        j.agent_view_authorized_scopes("alice", "reader", auth)
            .await
            .is_err(),
        "closing a Journal revokes its prior workspace signals"
    );
    let restored_sources = vec![
        j.open_workspace("alice", &workspace.workspace_id)
            .expect("explicit source restoration"),
    ];
    let auth = AgentAuthority::read(&other_grant)
        .with_memory(&fresh)
        .with_source_workspaces(&restored_sources);
    j.agent_view_authorized_scopes("alice", "reader", auth)
        .await
        .expect("explicit fresh memory and source capabilities");
    j.revoke_workspace("alice", &workspace.workspace_id, &workspace.version)
        .expect("durable original consent revoke");
    assert!(
        j.agent_view_authorized_scopes("alice", "reader", auth)
            .await
            .is_err(),
        "old capability object cannot override persistent revocation"
    );
    let manual = vec![
        WorkspaceGrant::open(
            "alice",
            "manual-after-revocation",
            "policy-v1",
            f.workspace.path(),
            &[f.private.path().into()],
        )
        .expect("owned fixture OS read only"),
    ];
    assert!(!manual[0].is_revoked());
    assert!(
        j.agent_view_authorized_scopes(
            "alice",
            "reader",
            AgentAuthority::read(&other_grant)
                .with_memory(&fresh)
                .with_source_workspaces(&manual)
        )
        .await
        .is_err(),
        "a new raw directory capability cannot erase the original persisted ACL withdrawal"
    );
    assert!(
        j.agent_view("alice", "reader")
            .expect("metadata")
            .final_text
            .is_none()
    );
}
#[tokio::test]
async fn lineage_uses_the_actual_model_input_and_ignores_later_unrelated_file_reads() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![memory(&mut j)];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![
        write("user supplied preference"),
        tools(vec![call("later", "read_file", json!({"path":"note.txt"}))]),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &f.config,
        vec![Message::plain(
            Role::User,
            "Remember my stated preference.".into(),
        )],
        auth,
        false,
    )
    .expect("start");
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("intent");
    let p = j
        .agent_memory_preview("alice", "run-1", &v.pending[0].operation_id, auth)
        .await
        .expect("preview");
    assert!(p.provenance.sources.is_empty());
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &p.operation_id,
            preview_digest: &p.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("write");
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("later read");
    j.approve_agent_tool(
        "alice",
        "run-1",
        &v.pending[0].operation_id,
        now().expect("time") + 60,
        &f.grant,
    )
    .expect("approve later read");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
        .await
        .expect("read later");
    std::fs::remove_file(f.workspace.path().join("note.txt"))
        .expect("delete unrelated later input");
    let other = tempfile::tempdir().expect("other");
    let read = WorkspaceGrant::open(
        "alice",
        "other",
        "policy-v1",
        other.path(),
        &[f.private.path().into()],
    )
    .expect("grant");
    let mut c = f.config.clone();
    c.run_id = "independent-reader".into();
    c.environment = read.environment().into();
    let auth = AgentAuthority::read(&read).with_memory(&grants);
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "memory",
            "memory_read",
            json!({"scope_id":"personal","entry_id":p.review.entry_id}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(Role::User, "Recall the preference.".into())],
        auth,
        false,
    )
    .expect("start");
    let v = j
        .advance_agent_authorized("alice", &c.run_id, auth, &model, rx.clone())
        .await
        .expect("read intent");
    j.approve_agent_memory(
        "alice",
        &c.run_id,
        AgentMemoryApproval {
            operation_id: &v.pending[0].operation_id,
            preview_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.advance_agent_authorized("alice", &c.run_id, auth, &model, rx.clone())
        .await
        .expect("no false dependency on future read");
    assert_eq!(
        j.advance_agent_authorized("alice", &c.run_id, auth, &model, rx)
            .await
            .expect("complete")
            .phase,
        Phase::Completed
    );
}
#[tokio::test]
async fn transitive_memory_requires_proof_only_scopes_and_original_file_authority() {
    let f = Fixture::new();
    let mut j = f.open();
    let first = vec![memory(&mut j)];
    let a = learn_file(&mut j, &f.config, &f.grant, &first).await;
    assert!(
        matches!(
            j.read_memory(&first[0], &a, None, 0, 4096, false),
            Err(crate::memory::MemoryError::SourceRequired)
        ),
        "raw store cannot bypass a file dependency"
    );
    assert!(
        j.list_memory(&first[0], None, 100, false).is_err(),
        "titles also need source authorization"
    );
    j.create_memory_scope("alice", "derived", false)
        .expect("second scope");
    let grants = vec![
        j.authorize_memory("alice", "personal", "policy-v1", false)
            .expect("source memory"),
        j.authorize_memory("alice", "derived", "policy-v1", true)
            .expect("target memory"),
    ];
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let mut c = f.config.clone();
    c.run_id = "derive".into();
    let mut proposal = write("Derived from the authorized note");
    proposal.tool_calls[0].arguments["scope_id"] = json!("derived");
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "read-parent",
            "memory_read",
            json!({"scope_id":"personal","entry_id":a}),
        )]),
        proposal,
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(Role::User, "Derive a second memory.".into())],
        auth,
        false,
    )
    .expect("start");
    let v = j
        .advance_agent_authorized("alice", "derive", auth, &model, rx.clone())
        .await
        .expect("read intent");
    j.approve_agent_memory(
        "alice",
        "derive",
        AgentMemoryApproval {
            operation_id: &v.pending[0].operation_id,
            preview_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.advance_agent_authorized("alice", "derive", auth, &model, rx.clone())
        .await
        .expect("read parent");
    let v = j
        .advance_agent_authorized("alice", "derive", auth, &model, rx.clone())
        .await
        .expect("derive intent");
    let p = j
        .agent_memory_preview("alice", "derive", &v.pending[0].operation_id, auth)
        .await
        .expect("causal memory reference");
    assert!(
        matches!(p.provenance.sources.as_slice(),[MemorySource::Memory{scope,..}] if scope=="personal")
    );
    j.approve_agent_memory(
        "alice",
        "derive",
        AgentMemoryApproval {
            operation_id: &p.operation_id,
            preview_digest: &p.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve derived");
    j.advance_agent_authorized("alice", "derive", auth, &model, rx.clone())
        .await
        .expect("write derived");
    j.advance_agent_authorized("alice", "derive", auth, &model, rx)
        .await
        .expect("finish");
    let other = tempfile::tempdir().expect("reader root");
    let read = WorkspaceGrant::open(
        "alice",
        "reader-root",
        "policy-v1",
        other.path(),
        &[f.private.path().into()],
    )
    .expect("read");
    let selected = vec![
        j.authorize_memory("alice", "derived", "policy-v1", false)
            .expect("only derived exposed"),
    ];
    let proof_memories = vec![
        j.authorize_memory("alice", "personal", "policy-v1", false)
            .expect("separate parent proof"),
    ];
    let proof_files = vec![f.grant.clone()];
    let auth = AgentAuthority::read(&read).with_memory(&selected);
    let mut c = f.config.clone();
    c.run_id = "third".into();
    c.environment = read.environment().into();
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "read-derived",
            "memory_read",
            json!({"scope_id":"derived","entry_id":p.review.entry_id}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(Role::User, "Read derived memory.".into())],
        auth,
        false,
    )
    .expect("start");
    let v = j
        .advance_agent_authorized("alice", "third", auth, &model, rx.clone())
        .await
        .expect("intent");
    j.approve_agent_memory(
        "alice",
        "third",
        AgentMemoryApproval {
            operation_id: &v.pending[0].operation_id,
            preview_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve read");
    assert!(
        j.advance_agent_authorized(
            "alice",
            "third",
            auth.with_source_workspaces(&proof_files),
            &model,
            rx.clone()
        )
        .await
        .is_err()
    );
    assert!(
        j.advance_agent_authorized(
            "alice",
            "third",
            auth.with_source_memories(&proof_memories),
            &model,
            rx.clone()
        )
        .await
        .is_err()
    );
    let complete = auth
        .with_source_workspaces(&proof_files)
        .with_source_memories(&proof_memories);
    j.advance_agent_authorized("alice", "third", complete, &model, rx.clone())
        .await
        .expect("full transitive proof");
    j.advance_agent_authorized("alice", "third", complete, &model, rx)
        .await
        .expect("complete");
    assert_eq!(
        model.requests.lock().expect("requests")[0]
            .tools
            .iter()
            .find(|v| v.name == "memory_read")
            .expect("memory tool")
            .parameters["properties"]["scope_id"]["enum"],
        json!(["derived"]),
        "proof-only authority is not a new tool grant"
    );
    j.register_run("alice", "delete-parent", "policy-v1")
        .expect("user action");
    let d = j
        .prepare_memory_write(
            &first[0],
            "delete-parent",
            crate::memory::MemoryWriteRequest {
                entry_id: a,
                expected_version: Some("1".into()),
                source_run: "delete-parent".into(),
                origin: crate::memory::MemoryOrigin::UserDeclaration,
                mutation: crate::memory::MemoryMutation::Delete,
                payload: None,
            },
        )
        .expect("delete preview");
    let hash = d.view(&j).expect("preview").digest.clone();
    j.approve_memory_write(d, &hash)
        .expect("delete root memory");
    assert!(
        j.agent_view_authorized_scopes("alice", "third", complete)
            .await
            .is_err(),
        "derived content cannot survive deletion of its ancestor"
    );
    assert!(
        j.agent_view("alice", "third")
            .expect("metadata")
            .final_text
            .is_none()
    );
}
#[tokio::test]
async fn runtime_draft_binding_is_atomic_and_source_acl_is_checked_inside_fact_transaction() {
    for fault in ["draft-binding", "late-acl"] {
        let mut f = Fixture::new();
        let mut j = f.open();
        let ws = j
            .authorize_workspace("alice", "source", f.workspace.path(), "policy-v1")
            .expect("consent");
        f.grant = j.open_workspace("alice", &ws.workspace_id).expect("scope");
        f.config.environment = f.grant.environment().into();
        let grants = vec![memory(&mut j)];
        let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
        let model = ScriptedModel::new(vec![
            tools(vec![call("file", "read_file", json!({"path":"note.txt"}))]),
            write("allowed data"),
        ]);
        let (_tx, rx) = watch::channel(false);
        j.start_agent_authorized(
            &f.config,
            vec![Message::plain(Role::User, "Read and learn.".into())],
            auth,
            false,
        )
        .expect("start");
        let v = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("file");
        j.approve_agent_tool(
            "alice",
            "run-1",
            &v.pending[0].operation_id,
            now().expect("time") + 60,
            &f.grant,
        )
        .expect("approve");
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("read");
        let v = j
            .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
            .await
            .expect("memory");
        let op = &v.pending[0].operation_id;
        if fault == "draft-binding" {
            j.connection.execute_batch("CREATE TRIGGER binding_fault BEFORE UPDATE ON runtime_memory_operations WHEN NEW.draft_id IS NOT NULL BEGIN SELECT RAISE(ABORT,'owned binding fault');END;").expect("fault");
            assert!(
                j.agent_memory_preview("alice", "run-1", op, auth)
                    .await
                    .is_err()
            );
            let count: i64 = j
                .connection
                .query_row("SELECT count(*) FROM memory_drafts", [], |r| r.get(0))
                .expect("no orphan");
            assert_eq!(count, 0);
        } else {
            let p = j
                .agent_memory_preview("alice", "run-1", op, auth)
                .await
                .expect("preview");
            j.approve_agent_memory(
                "alice",
                "run-1",
                AgentMemoryApproval {
                    operation_id: op,
                    preview_digest: &p.approval_digest,
                    expires_at: now().expect("time") + 60,
                },
                auth,
            )
            .await
            .expect("approval");
            j.connection.execute_batch("CREATE TRIGGER source_acl_fault AFTER INSERT ON memory_entries BEGIN UPDATE local_workspaces SET active=0,version=version+1;END;").expect("owned late ACL change");
            assert!(
                j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
                    .await
                    .is_err()
            );
            let count: i64 = j
                .connection
                .query_row("SELECT count(*) FROM memory_entries", [], |r| r.get(0))
                .expect("no fact");
            assert_eq!(count, 0);
            assert_eq!(
                j.agent_memory_metadata("alice", "run-1", op)
                    .expect("metadata")
                    .expect("draft")
                    .state,
                "pending"
            );
        }
    }
}
#[tokio::test]
async fn rehashed_empty_lineage_cannot_erase_the_actual_model_input_dependencies() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![memory(&mut j)];
    let entry = learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let(op,body):(String,String)=j.connection.query_row("SELECT m.operation_id,l.body FROM runtime_memory_operations m JOIN memory_drafts d USING(draft_id) JOIN runtime_memory_lineage l USING(operation_id) WHERE m.scope_id='personal' AND m.entry_id=? AND d.state='approved'",[&entry],|r|Ok((r.get(0)?,r.get(1)?))).expect("lineage");
    let mut value: serde_json::Value = serde_json::from_str(&body).expect("json");
    value["sources"] = json!([]);
    let sha = digest("memory-provenance.v1", &value).expect("rehash fixture");
    assert!(
        j.connection
            .execute(
                "UPDATE runtime_memory_lineage SET body=?,digest=? WHERE operation_id=?",
                params![encode(&value).expect("body"), sha, op]
            )
            .is_err(),
        "normal mutation is blocked"
    );
    j.connection
        .execute_batch("DROP TRIGGER memory_lineage_binding;")
        .expect("simulate corrupt private state");
    j.connection
        .execute(
            "UPDATE runtime_memory_lineage SET body=?,digest=? WHERE operation_id=?",
            params![encode(&value).expect("body"), sha, op],
        )
        .expect("owned corruption fixture");
    assert!(
        j.read_memory(&grants[0], &entry, None, 0, 4096, false)
            .is_err(),
        "raw API independently checks the actual request history"
    );
    assert!(
        j.agent_view_authorized_scopes(
            "alice",
            "run-1",
            AgentAuthority::read(&f.grant).with_memory(&grants)
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn changed_file_during_generation_never_commits_a_derived_model_response() {
    struct Delayed {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl ModelGateway for Delayed {
        fn generate<'a>(
            &'a self,
            _: &'a GenerateRequest,
            _: watch::Receiver<bool>,
        ) -> ModelFuture<'a> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(ResponseCandidate {
                    text: "Stale derived text must not be published".into(),
                    ..final_response()
                })
            })
        }
    }
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![memory(&mut j)];
    let entry = learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let mut c = f.config.clone();
    c.run_id = "delayed-reader".into();
    let auth = AgentAuthority::read(&f.grant).with_memory(&grants);
    let model = ScriptedModel::new(vec![tools(vec![call(
        "read",
        "memory_read",
        json!({"scope_id":"personal","entry_id":entry}),
    )])]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(Role::User, "Use the note.".into())],
        auth,
        false,
    )
    .expect("start");
    let v = j
        .advance_agent_authorized("alice", &c.run_id, auth, &model, rx.clone())
        .await
        .expect("intent");
    j.approve_agent_memory(
        "alice",
        &c.run_id,
        AgentMemoryApproval {
            operation_id: &v.pending[0].operation_id,
            preview_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.advance_agent_authorized("alice", &c.run_id, auth, &model, rx.clone())
        .await
        .expect("read verified source");
    let delayed = Delayed {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    };
    let work = j.advance_agent_authorized("alice", &c.run_id, auth, &delayed, rx);
    let change = async {
        delayed.entered.notified().await;
        std::fs::write(
            f.workspace.path().join("note.txt"),
            "changed while generating",
        )
        .expect("owned external writer");
        delayed.release.notify_one();
    };
    let (result, _) = tokio::join!(work, change);
    let view = result.expect("metadata only");
    assert_eq!(view.phase, Phase::RecoveryRequired);
    assert!(view.final_text.is_none());
    let responses: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM responses WHERE run_id=?",
            [&c.run_id],
            |r| r.get(0),
        )
        .expect("committed replies");
    assert_eq!(
        responses, 1,
        "only the original tool proposal was committed"
    );
}
