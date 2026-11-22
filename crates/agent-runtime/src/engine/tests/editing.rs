use super::*;
mod recovery_scopes;
mod retention_export;
mod retention_scopes;
use crate::workspace::patch::authority::EditFileGrant;
use sha2::{Digest, Sha256};
fn edit(j: &mut Journal, f: &Fixture) -> EditFileGrant {
    j.authorize_file_edit(
        "alice",
        "separate-edit-consent",
        "policy-v1",
        f.workspace.path(),
    )
    .expect("explicit edit grant")
}
fn authority<'a>(f: &'a Fixture, edit: &'a EditFileGrant) -> EditingAuthority<'a> {
    EditingAuthority {
        read: &f.grant,
        create: None,
        edit,
    }
}
fn patch_call(id: &str, path: &str, old: &str, new: &str) -> ResponseCandidate {
    tools(vec![call(
        id,
        "apply_patch",
        json!({"path":path,"expected_sha256":hex::encode(Sha256::digest(format!("{old}\n"))),"patch":format!("@@ -1 +1 @@\n-{old}\n+{new}\n")}),
    )])
}
async fn advance(
    j: &mut Journal,
    f: &Fixture,
    edit: &EditFileGrant,
    model: &impl ModelGateway,
    rx: watch::Receiver<bool>,
) -> RunView {
    j.advance_agent_editing("alice", "run-1", authority(f, edit), model, rx)
        .await
        .expect("advance editing Run")
}
async fn approve_patch(j: &mut Journal, f: &Fixture, edit: &EditFileGrant, view: &RunView) {
    let op = &view.pending[0];
    assert!(
        j.approve_agent_tool(
            "alice",
            "run-1",
            &op.operation_id,
            f.config.deadline_unix,
            &f.grant
        )
        .is_err()
    );
    let preview = j
        .agent_patch_preview("alice", "run-1", &op.operation_id, authority(f, edit))
        .await
        .expect("actual exact preview");
    assert!(
        j.approve_agent_editing(
            "alice",
            "run-1",
            EditingApproval {
                operation_id: &op.operation_id,
                preview_digest: &"0".repeat(64),
                expires_at: f.config.deadline_unix
            },
            authority(f, edit)
        )
        .await
        .is_err()
    );
    j.approve_agent_editing(
        "alice",
        "run-1",
        EditingApproval {
            operation_id: &op.operation_id,
            preview_digest: &preview.preview_digest,
            expires_at: f.config.deadline_unix,
        },
        authority(f, edit),
    )
    .await
    .expect("exact owner preview approval");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_read_edit_edit_final_and_reopen_preserve_context_without_replaying_effects() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    let consent = edit.consent_id().to_owned();
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(
            Role::User,
            "Read and edit note.txt twice".into(),
        )],
        authority(&f, &edit),
        false,
    )
    .expect("opt in explicitly");
    let model = ScriptedModel::new(vec![
        tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
        patch_call("edit1", "note.txt", "old", "middle"),
        patch_call("edit2", "note.txt", "middle", "latest"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let read = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(read.phase, Phase::ToolsPending);
    f.approve(&j);
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let first = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert!(
        f.grant
            .prepare(
                "alice",
                f.grant.environment(),
                "policy-v1",
                first.pending[0]
                    .request
                    .workspace()
                    .expect("filesystem request")
                    .clone()
            )
            .await
            .is_err()
    );
    approve_patch(&mut j, &f, &edit, &first).await;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("first actual effect"),
        b"middle\n"
    );
    let second = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(second.phase, Phase::ToolsPending);
    drop(j);
    let mut j = f.open();
    assert!(
        j.advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx.clone())
            .await
            .is_err()
    );
    let edit = j
        .restore_file_edit("alice", &consent)
        .expect("explicit host reacquired OS access fixture");
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    approve_patch(&mut j, &f, &edit, &second).await;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let done = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(done.phase, Phase::Completed);
    assert_eq!((done.steps, done.invocations), (7, 4));
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("second actual effect"),
        b"latest\n"
    );
    assert_eq!(
        std::fs::read_dir(f.workspace.path())
            .expect("originals retained")
            .count(),
        3
    );
    let view = j
        .agent_view_authorized("alice", "run-1", &f.grant)
        .await
        .expect("current authorized history");
    assert!(view.final_text.is_some());
    {
        let requests = model.requests.lock().expect("requests");
        assert!(
            requests
                .iter()
                .all(|r| r.tools.iter().any(|t| t.name == "apply_patch"))
        );
        assert!(
            requests
                .iter()
                .flat_map(|r| &r.messages)
                .filter(|m| m.role == Role::Tool)
                .all(|m| !m.content.contains("retained_original")
                    && !m.content.contains(".orange-agent-tmp-"))
        );
    }

    let again = advance(&mut j, &f, &edit, &model, rx).await;
    assert_eq!(again.steps, 7);
    assert_eq!(model.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_create_then_edit_uses_both_independent_capabilities() {
    let f = Fixture::new();
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    let create = j
        .authorize_file_creation("alice", "create-consent", "policy-v1", f.workspace.path())
        .expect("independent create consent");
    let auth = EditingAuthority {
        read: &f.grant,
        create: Some(&create),
        edit: &edit,
    };
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Create and edit new.txt".into())],
        auth,
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "create",
            "write_file",
            json!({"path":"new.txt","content":"old\n"}),
        )]),
        patch_call("edit", "new.txt", "old", "edited"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let pending = j
        .advance_agent_editing("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("creation proposal");
    let op = &pending.pending[0];
    j.approve_agent_creation(
        "alice",
        "run-1",
        CreationApproval {
            operation_id: &op.operation_id,
            argument_digest: &op.argument_digest,
            expires_at: f.config.deadline_unix,
        },
        &f.grant,
        &create,
    )
    .await
    .expect("create approval");
    j.advance_agent_editing("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("create effect");
    let pending = j
        .advance_agent_editing("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("edit proposal");
    let op = &pending.pending[0];
    let preview = j
        .agent_patch_preview("alice", "run-1", &op.operation_id, auth)
        .await
        .expect("edit preview");
    j.approve_agent_editing(
        "alice",
        "run-1",
        EditingApproval {
            operation_id: &op.operation_id,
            preview_digest: &preview.preview_digest,
            expires_at: f.config.deadline_unix,
        },
        auth,
    )
    .await
    .expect("edit approval");
    j.advance_agent_editing("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("edit effect");
    let done = j
        .advance_agent_editing("alice", "run-1", auth, &model, rx)
        .await
        .expect("final model after edited creation source");
    assert_eq!(done.phase, Phase::Completed);
    assert_eq!(done.steps, 5);
    assert_eq!(
        std::fs::read(f.workspace.path().join("new.txt")).expect("actual bytes"),
        b"edited\n"
    );
}

#[tokio::test]
async fn readonly_run_never_gains_an_edit_tool_and_unsafe_model_batch_has_zero_effects() {
    let f = Fixture::new();
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    f.start(&mut j);
    assert!(
        j.agent_edit_requirement("alice", "run-1")
            .expect("requirement")
            .is_none()
    );
    assert!(
        j.start_agent_editing(
            &f.config,
            vec![Message::plain(Role::User, "Read note.txt".into())],
            authority(&f, &edit),
            false
        )
        .is_err()
    );
    let model = ScriptedModel::new(vec![patch_call(
        "unauthorized",
        "note.txt",
        "allowed data",
        "bad",
    )]);
    let (_tx, rx) = watch::channel(false);
    assert!(f.advance(&mut j, &model, rx).await.is_err());
    assert!(
        !model.requests.lock().expect("requests")[0]
            .tools
            .iter()
            .any(|t| t.name == "apply_patch")
    );
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("note.txt")).expect("unchanged"),
        "allowed data"
    );
    let n: i64 = j
        .connection
        .query_row("SELECT count(*) FROM operations", [], |r| r.get(0))
        .expect("no operations");
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_level_patch_approval_cannot_bypass_normal_tool_contract_pin() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit".into())],
        authority(&f, &edit),
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![patch_call("edit", "note.txt", "old", "new")]);
    let (_tx, rx) = watch::channel(false);
    let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let id = pending.pending[0].operation_id.clone();
    let preview = j
        .agent_patch_preview("alice", "run-1", &id, authority(&f, &edit))
        .await
        .expect("preview");
    let op = j.operation_record(&f.config, &id).expect("operation");
    j.approve_patch_review(
        &edit,
        &f.grant,
        &op.operation(&f.config),
        &preview.preview_digest,
        "alice",
        f.config.deadline_unix,
    )
    .await
    .expect("low-level exact approval");
    assert!(
        j.begin_patch_publication(&edit, &f.grant, &op.operation(&f.config))
            .await
            .is_err()
    );
    assert!(
        j.advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx)
            .await
            .is_err()
    );
    assert_eq!(
        j.state("alice", &id).expect("not dispatched").as_deref(),
        Some("intended")
    );
    assert_eq!(
        std::fs::read_dir(f.workspace.path())
            .expect("zero write effects")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("source"),
        b"old\n"
    );
}

#[tokio::test]
async fn current_thread_host_refuses_publication_before_durable_dispatch() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit".into())],
        authority(&f, &edit),
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![patch_call("edit", "note.txt", "old", "new")]);
    let (_tx, rx) = watch::channel(false);
    let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    approve_patch(&mut j, &f, &edit, &pending).await;
    assert!(matches!(
        j.advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx)
            .await,
        Err(EngineError::ExecutionEnvironmentUnavailable)
    ));
    assert_eq!(
        j.state("alice", &pending.pending[0].operation_id)
            .expect("state")
            .as_deref(),
        Some("intended")
    );
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM editing_publications", [], |r| {
            r.get(0)
        })
        .expect("no dispatch");
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budgets_cancellation_and_edit_revocation_prevent_unapproved_effects() {
    for case in ["budget", "cancel", "revoke", "external-change"] {
        let mut f = Fixture::new();
        std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
        if case == "budget" {
            f.config.max_steps = 1;
        }
        let mut j = f.open();
        let edit = edit(&mut j, &f);
        j.start_agent_editing(
            &f.config,
            vec![Message::plain(Role::User, "Edit".into())],
            authority(&f, &edit),
            false,
        )
        .expect("start");
        let model = ScriptedModel::new(vec![patch_call("edit", "note.txt", "old", "new")]);
        let (tx, rx) = watch::channel(false);
        let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
        approve_patch(&mut j, &f, &edit, &pending).await;
        match case {
            "cancel" => tx.send(true).expect("cancel"),
            "revoke" => {
                j.revoke_file_edit(
                    "alice",
                    edit.consent_id(),
                    &edit.consent_version().expect("version"),
                )
                .expect("revoke");
            }
            "external-change" => std::fs::write(f.workspace.path().join("note.txt"), b"external\n")
                .expect("external edit"),
            _ => {}
        }
        let result = j
            .advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx)
            .await;
        if case == "budget" {
            assert_eq!(result.expect("limited").phase, Phase::Limited);
        } else if case == "cancel" {
            assert_eq!(result.expect("cancelled").phase, Phase::Cancelled);
        } else {
            assert!(result.is_err());
        }
        let expected = if case == "external-change" {
            &b"external\n"[..]
        } else {
            &b"old\n"[..]
        };
        assert_eq!(
            std::fs::read(f.workspace.path().join("note.txt")).expect("not modified by agent"),
            expected
        );
        let n: i64 = j
            .connection
            .query_row("SELECT count(*) FROM editing_publications", [], |r| {
                r.get(0)
            })
            .expect("no effect dispatch");
        assert_eq!(n, 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_failure_never_replays_patch_and_missing_capabilities_are_not_synthesized() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    let id = edit.consent_id().to_owned();
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit".into())],
        authority(&f, &edit),
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![patch_call("edit", "note.txt", "old", "new")]);
    let (_tx, rx) = watch::channel(false);
    let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    approve_patch(&mut j, &f, &edit, &pending).await;
    assert!(
        j.agent_unresolved_patch_ids("alice", "run-1", None, 10)
            .expect("intended is not unknown")
            .is_empty()
    );
    j.connection.execute_batch("CREATE TRIGGER fail_edit_edge BEFORE INSERT ON patch_version_edges BEGIN SELECT RAISE(ABORT,'owned receipt fixture');END;").expect("fault");
    let failed = advance(&mut j, &f, &edit, &model, rx).await;
    assert_eq!(failed.phase, Phase::RecoveryRequired);
    assert_eq!(
        j.state("alice", &pending.pending[0].operation_id)
            .expect("unknown")
            .as_deref(),
        Some("unknown")
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("effect retained"),
        b"new\n"
    );
    drop(j);
    let mut j = f.open();
    assert!(j.restore_file_edit("alice", &id).is_err());
    let operation = &pending.pending[0].operation_id;
    assert!(
        j.authorize_patch_retention("alice", "run-1", operation, f.workspace.path())
            .is_err(),
        "unresolved unknown cannot release active reservation"
    );
    assert!(
        j.agent_retention_patch_ids("alice", "run-1", None, 10)
            .expect("retention index")
            .is_empty()
    );
    assert_eq!(
        j.agent_unresolved_patch_ids("alice", "run-1", None, 1)
            .expect("discover unknown"),
        vec![operation.clone()]
    );
    assert!(
        j.agent_unresolved_patch_ids("bob", "run-1", None, 10)
            .is_err()
    );
    assert!(
        j.agent_unresolved_patch_ids("alice", "other-run", None, 10)
            .is_err()
    );
    for (cursor, limit) in [(None, 0), (None, 101), (Some("bad cursor"), 1)] {
        assert!(
            j.agent_unresolved_patch_ids("alice", "run-1", cursor, limit)
                .is_err()
        );
    }
    assert!(
        j.agent_unresolved_patch_ids("alice", "run-1", Some(operation), 1)
            .expect("strict cursor")
            .is_empty()
    );
    j.revoke_run("alice", "run-1")
        .expect("cancel must not hide unresolved effects");
    assert_eq!(
        j.agent_unresolved_patch_ids("alice", "run-1", None, 1)
            .expect("revoked metadata only"),
        vec![operation.clone()]
    );
    assert!(
        j.restore_file_edit("alice", &id).is_err(),
        "index cannot restore write access"
    );
    j = recovery_scopes::resolve(j, &f, operation).await;
    let retention = j
        .authorize_patch_retention("alice", "run-1", operation, f.workspace.path())
        .expect("resolved unknown can be managed separately");
    assert_eq!(
        j.agent_retention_patch_ids("alice", "run-1", None, 10)
            .expect("resolved retention index"),
        vec![operation.clone()]
    );
    let review = j
        .prepare_selected_retention(&retention, "retain-resolved")
        .await
        .expect("retention preview");
    j.approve_selected_retention(&retention, &review.review_id, &review.digest)
        .await
        .expect("explicit retention after resolution");
    assert_eq!(
        j.agent_retention_review_id("alice", "run-1", operation)
            .expect("content-free lost-reply lookup"),
        Some(review.review_id)
    );
    assert!(
        j.agent_unresolved_patch_ids("alice", "run-1", None, 10)
            .expect("resolved excluded")
            .is_empty()
    );
    assert_eq!(
        j.state("alice", operation)
            .expect("original operation still unknown")
            .as_deref(),
        Some("unknown")
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("no original operation replay"),
        b"new\n"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_dir(f.workspace.path())
            .expect("two versions retained")
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_binding_is_immutable_and_mixed_invalid_response_commits_nothing() {
    let f = Fixture::new();
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit".into())],
        authority(&f, &edit),
        false,
    )
    .expect("start");
    assert!(
        j.connection
            .execute("UPDATE runtime_edit_capabilities SET consent_version=2", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM runtime_edit_capabilities", [])
            .is_err()
    );
    let mut invalid = patch_call("edit", "note.txt", "allowed data", "new");
    invalid.tool_calls.push(call("escape","apply_patch",json!({"path":"../outside","expected_sha256":"0".repeat(64),"patch":"@@ -1 +1 @@\n-old\n+new\n"})));
    let model = ScriptedModel::new(vec![invalid]);
    let (_tx, rx) = watch::channel(false);
    assert!(
        j.advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx)
            .await
            .is_err()
    );
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM responses", [], |r| r.get(0))
        .expect("no complete response accepted");
    assert_eq!(count, 0);
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM operations", [], |r| r.get(0))
        .expect("no partial batch");
    assert_eq!(count, 0);
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("note.txt")).expect("unchanged"),
        "allowed data"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_tool_deadline_are_observed_during_blocking_publication() {
    for timeout in [false, true] {
        let f = Fixture::new();
        std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
        let mut j = f.open();
        let edit = edit(&mut j, &f);
        j.start_agent_editing(
            &f.config,
            vec![Message::plain(Role::User, "Edit".into())],
            authority(&f, &edit),
            false,
        )
        .expect("start");
        let model = ScriptedModel::new(vec![patch_call("edit", "note.txt", "old", "new")]);
        let (tx, rx) = watch::channel(false);
        let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
        approve_patch(&mut j, &f, &edit, &pending).await;
        let stored = j.stored_run("alice", "run-1").expect("run");
        let op = j
            .operation_record(&f.config, &pending.pending[0].operation_id)
            .expect("operation");
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        let cancel_task = if timeout {
            None
        } else {
            Some(tokio::spawn(async move {
                signal.notified().await;
                tx.send(true).expect("cancel during owned blocking stage");
            }))
        };
        let started = std::time::Instant::now();
        j.advance_edit_tool_with_hook(&stored, &op, authority(&f, &edit).internal(), rx, |stage| {
            if stage == 2 {
                entered.notify_one();
                let stop = std::time::Instant::now() + Duration::from_secs(15);
                while !f.grant.is_revoked() {
                    assert!(
                        std::time::Instant::now() < stop,
                        "host revocation watcher did not run"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(())
        })
        .await
        .expect("bounded stop");
        if let Some(task) = cancel_task {
            task.await.expect("cancel task");
        }
        let view = j.agent_view("alice", "run-1").expect("metadata");
        if timeout {
            assert_eq!(view.phase, Phase::RecoveryRequired);
            assert_eq!(view.failure.as_deref(), Some("edit_timeout"));
            assert!(started.elapsed() >= Duration::from_secs(9));
        } else {
            assert_eq!(view.phase, Phase::Cancelled);
        }
        assert_eq!(
            j.state("alice", &op.id)
                .expect("uncertain stage")
                .as_deref(),
            Some("unknown")
        );
        assert_eq!(
            std::fs::read(f.workspace.path().join("note.txt")).expect("target not replaced"),
            b"old\n"
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edited_source_changes_and_forged_model_projection_block_the_next_model_call() {
    for case in ["deleted", "same-bytes-new-inode", "forged-projection"] {
        let f = Fixture::new();
        std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
        let mut j = f.open();
        let edit = edit(&mut j, &f);
        j.start_agent_editing(
            &f.config,
            vec![Message::plain(Role::User, "Edit".into())],
            authority(&f, &edit),
            false,
        )
        .expect("start");
        let model = ScriptedModel::new(vec![
            patch_call("edit", "note.txt", "old", "new"),
            final_response(),
        ]);
        let (_tx, rx) = watch::channel(false);
        let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
        approve_patch(&mut j, &f, &edit, &pending).await;
        advance(&mut j, &f, &edit, &model, rx.clone()).await;
        let target = f.workspace.path().join("note.txt");
        match case {
            "deleted" => std::fs::remove_file(&target).expect("owned deletion"),
            "same-bytes-new-inode" => {
                std::fs::rename(&target, f.workspace.path().join("external-saved"))
                    .expect("retain prior inode");
                std::fs::write(&target, b"new\n").expect("new external inode");
            }
            "forged-projection" => {
                let raw: String = j
                    .connection
                    .query_row(
                        "SELECT result_json FROM operations WHERE operation_id=?",
                        [&pending.pending[0].operation_id],
                        |r| r.get(0),
                    )
                    .expect("actual receipt");
                let value: serde_json::Value = serde_json::from_str(&raw).expect("JSON");
                j.connection
                    .execute(
                        "UPDATE operations SET model_result=? WHERE operation_id=?",
                        params![
                            serde_json::to_string(&json!({"kind":"apply_patch","data":value}))
                                .expect("forged full durable projection"),
                            pending.pending[0].operation_id
                        ],
                    )
                    .expect("owned corrupted cache fixture");
            }
            _ => unreachable!(),
        }
        let view = advance(&mut j, &f, &edit, &model, rx).await;
        assert_eq!(view.phase, Phase::RecoveryRequired, "{case}");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1, "{case}");
        assert!(view.final_text.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn official_patch_template_binds_the_read_and_exact_write_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let mut f = Fixture::new();
    f.config.model_version = "qwen7b-edit-candidate-v1".into();
    let source = f.workspace.path().join("note.txt");
    std::fs::write(&source, b"old\n")?;
    let mut j = f.open();
    let selection = j
        .skills("alice")?
        .into_iter()
        .find(|s| s.pin.id == "orange.reviewed-patch")
        .expect("official patch");
    let pin = selection.pin;
    j.set_skill_enabled("alice", &pin, 0, true)?;
    assert!(
        j.start_agent_skill(&f.config, &pin, "Edit note.txt".into(), &f.grant)
            .is_err()
    );
    let edit = edit(&mut j, &f);
    j.start_agent_patch_skill(
        &f.config,
        &pin,
        Some(1),
        "Edit note.txt".into(),
        authority(&f, &edit),
    )?;
    let model = ScriptedModel::new(vec![
        tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
        patch_call("patch", "note.txt", "old", "new"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let read = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(read.pending.len(), 1);
    j.approve_agent_tool(
        "alice",
        "run-1",
        &read.pending[0].operation_id,
        f.config.deadline_unix,
        &f.grant,
    )?;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let proposal = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let op = &proposal.pending[0];
    assert_eq!(std::fs::read(&source)?, b"old\n");
    assert!(
        j.advance_agent_editing("alice", "run-1", authority(&f, &edit), &model, rx.clone())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&source)?, b"old\n");
    assert_eq!(op.request.workspace().expect("patch").name(), "apply_patch");
    approve_patch(&mut j, &f, &edit, &proposal).await;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let done = advance(&mut j, &f, &edit, &model, rx).await;
    assert_eq!(done.phase, Phase::Completed);
    assert_eq!(std::fs::read(&source)?, b"new\n");
    let requests = model.requests.lock().expect("model request inventory");
    for q in requests.iter() {
        assert_eq!(
            q.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            ["apply_patch", "read_file"].into_iter().collect()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn official_patch_template_rejects_unread_content_and_empty_completion()
-> Result<(), Box<dyn std::error::Error>> {
    for guessed in [true, false] {
        let mut f = Fixture::new();
        f.config.model_version = "qwen7b-edit-candidate-v1".into();
        let source = f.workspace.path().join("note.txt");
        std::fs::write(&source, b"old\n")?;
        let mut j = f.open();
        let pin = j
            .skills("alice")?
            .into_iter()
            .find(|s| s.pin.id == "orange.reviewed-patch")
            .expect("official patch")
            .pin;
        j.set_skill_enabled("alice", &pin, 0, true)?;
        let grant = edit(&mut j, &f);
        j.start_agent_patch_skill(
            &f.config,
            &pin,
            Some(1),
            "Edit note.txt".into(),
            authority(&f, &grant),
        )?;
        let reply = if guessed {
            patch_call("guessed", "note.txt", "old", "new")
        } else {
            final_response()
        };
        let model = ScriptedModel::new(vec![reply]);
        let (_tx, rx) = watch::channel(false);
        let result = j
            .advance_agent_editing("alice", "run-1", authority(&f, &grant), &model, rx)
            .await;
        if guessed {
            assert!(matches!(result, Err(EngineError::Forbidden)));
        } else {
            let view = result?;
            assert_eq!(view.phase, Phase::Failed);
            assert_eq!(view.failure.as_deref(), Some("skill_patch_required"));
            assert!(view.final_text.is_none());
        }
        assert_eq!(std::fs::read(source)?, b"old\n");
        let n: i64 = j.connection.query_row(
            "SELECT count(*) FROM operations WHERE run_id='run-1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(n, 0);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_patch_template_cannot_dispatch_an_already_approved_change()
-> Result<(), Box<dyn std::error::Error>> {
    let mut f = Fixture::new();
    f.config.model_version = "qwen7b-edit-candidate-v1".into();
    let source = f.workspace.path().join("note.txt");
    std::fs::write(&source, b"old\n")?;
    let mut j = f.open();
    let pin = j
        .skills("alice")?
        .into_iter()
        .find(|s| s.pin.id == "orange.reviewed-patch")
        .expect("official patch")
        .pin;
    j.set_skill_enabled("alice", &pin, 0, true)?;
    let edit = edit(&mut j, &f);
    j.start_agent_patch_skill(
        &f.config,
        &pin,
        Some(1),
        "Edit note.txt".into(),
        authority(&f, &edit),
    )?;
    let model = ScriptedModel::new(vec![
        tools(vec![call("read", "read_file", json!({"path":"note.txt"}))]),
        patch_call("patch", "note.txt", "old", "new"),
    ]);
    let (_tx, rx) = watch::channel(false);
    let read = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    j.approve_agent_tool(
        "alice",
        "run-1",
        &read.pending[0].operation_id,
        f.config.deadline_unix,
        &f.grant,
    )?;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    let proposal = advance(&mut j, &f, &edit, &model, rx.clone()).await;
    approve_patch(&mut j, &f, &edit, &proposal).await;
    j.set_skill_enabled("alice", &pin, 1, false)?;
    j.set_skill_enabled("alice", &pin, 2, true)?;
    let stopped = advance(&mut j, &f, &edit, &model, rx).await;
    assert_eq!(stopped.phase, Phase::Cancelled);
    assert_eq!(std::fs::read(source)?, b"old\n");
    Ok(())
}
