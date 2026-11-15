use super::*;
use crate::memory::{MemoryGrant, MemoryMutation, MemoryOrigin, MemoryPayload, MemoryWriteRequest};
use crate::workspace::creation::CreateFileArgs;
use std::sync::{Arc, Condvar};
fn seed(j: &mut Journal) -> MemoryGrant {
    j.create_memory_scope("alice", "personal", false)
        .expect("scope");
    j.register_run("alice", "seed", "policy-v1")
        .expect("user data");
    let g = j
        .authorize_memory("alice", "personal", "policy-v1", true)
        .expect("grant");
    let p = j
        .prepare_memory_write(
            &g,
            "seed",
            MemoryWriteRequest {
                entry_id: "preference".into(),
                expected_version: None,
                source_run: "seed".into(),
                origin: MemoryOrigin::UserDeclaration,
                mutation: MemoryMutation::Put,
                payload: Some(MemoryPayload {
                    title: "Preference".into(),
                    content: "remembered bytes".into(),
                    enabled: true,
                }),
            },
        )
        .expect("draft");
    let d = p.view(j).expect("view").digest.clone();
    j.approve_memory_write(p, &d).expect("user approved seed");
    g
}
async fn read_memory_step(
    j: &mut Journal,
    f: &Fixture,
    auth: AgentAuthority<'_>,
    model: &ScriptedModel,
    rx: watch::Receiver<bool>,
) {
    j.start_agent_authorized(
        &f.config,
        vec![Message::plain(
            Role::User,
            "Use my memory to update the file only with my approval.".into(),
        )],
        auth,
        false,
    )
    .expect("mixed capabilities");
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, model, rx.clone())
        .await
        .expect("memory read");
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &v.pending[0].operation_id,
            preview_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve read");
    j.advance_agent_authorized("alice", "run-1", auth, model, rx)
        .await
        .expect("actual memory read");
}
fn memory_call() -> ResponseCandidate {
    tools(vec![call(
        "memory",
        "memory_read",
        json!({"scope_id":"personal","entry_id":"preference"}),
    )])
}
fn create_call() -> ResponseCandidate {
    tools(vec![call(
        "create",
        "write_file",
        json!({"path":"from-memory.txt","content":"remembered bytes"}),
    )])
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_to_new_file_uses_current_sources_and_separate_file_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let create = j
        .authorize_file_creation("alice", "create", "policy-v1", f.workspace.path())
        .expect("separate create");
    let auth = AgentAuthority::read(&f.grant)
        .with_memory(&grants)
        .with_creation(&create);
    let model = ScriptedModel::new(vec![memory_call(), create_call(), final_response()]);
    let (_tx, rx) = watch::channel(false);
    read_memory_step(&mut j, &f, auth, &model, rx.clone()).await;
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("create proposal");
    let op = &v.pending[0];
    assert!(!f.workspace.path().join("from-memory.txt").exists());
    j.approve_agent_creation_authorized(
        "alice",
        "run-1",
        CreationApproval {
            operation_id: &op.operation_id,
            argument_digest: &op.argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("exact file approval");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("actual source-bound create");
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("from-memory.txt")).expect("file"),
        "remembered bytes"
    );
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("finish")
            .phase,
        Phase::Completed
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_creation_future_does_not_release_memory_or_journal_before_worker_exit() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let create = j
        .authorize_file_creation("alice", "create", "policy-v1", f.workspace.path())
        .expect("create");
    let auth = AgentAuthority::read(&f.grant)
        .with_memory(&grants)
        .with_creation(&create);
    let model = ScriptedModel::new(vec![memory_call(), create_call()]);
    let (_tx, rx) = watch::channel(false);
    read_memory_step(&mut j, &f, auth, &model, rx.clone()).await;
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx)
        .await
        .expect("intent");
    let id = v.pending[0].operation_id.clone();
    j.approve_agent_creation_authorized(
        "alice",
        "run-1",
        CreationApproval {
            operation_id: &id,
            argument_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    let stored = j.stored_run("alice", "run-1").expect("stored");
    let op = j.operation_record(&stored.config, &id).expect("op");
    let proof = j
        .revalidate_sources_authority(&stored.config, auth)
        .await
        .expect("source proof");
    let fence = proof.into_effect_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let signal = entered.clone();
    let paused = gate.clone();
    fence.before_for_test("create.before_publish", move || {
        signal.notify_one();
        let (lock, cond) = &*paused;
        let mut released = lock.lock().expect("gate");
        while !*released {
            released = cond.wait(released).expect("wait");
        }
    });
    let observer = fence.observer();
    {
        let operation = op.operation(&stored.config);
        let mut future = Box::pin(j.create_file_for_agent_with_sources(
            &create,
            &operation,
            CreateFileArgs {
                path: "from-memory.txt".into(),
                content: "remembered bytes".into(),
            },
            &f.grant,
            Some(fence),
        ));
        tokio::select! {_=entered.notified()=>{},result=&mut future=>panic!("worker unexpectedly finished: {result:?}")};
        grants[0].request_revoke();
        assert!(!observer.is_live());
        drop(future);
    }
    let revoker = j
        .memory_scope_revoker("alice", "personal", "1")
        .expect("scope revoker");
    revoker.request_revoke();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), revoker.wait_drained())
            .await
            .is_err(),
        "a dropped Future is not a stopped worker"
    );
    drop(j);
    assert!(
        matches!(
            Journal::open(&f.private.path().join("journal.sqlite")),
            Err(JournalError::Busy)
        ),
        "the explicit effect lease retains host ownership"
    );
    assert!(!f.workspace.path().join("from-memory.txt").exists());
    {
        let (lock, cond) = &*gate;
        *lock.lock().expect("release") = true;
        cond.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), revoker.wait_drained())
        .await
        .expect("actual worker drain");
    assert!(!f.workspace.path().join("from-memory.txt").exists());
    drop(observer);
    let mut reopened = None;
    for _ in 0..100 {
        match Journal::open(&f.private.path().join("journal.sqlite")) {
            Ok(j) => {
                reopened = Some(j);
                break;
            }
            Err(JournalError::Busy) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => panic!("reopen: {e}"),
        }
    }
    let j = reopened.expect("host released after worker");
    assert_eq!(
        j.state("alice", &id).expect("state").as_deref(),
        Some("unknown")
    );
}
#[cfg(target_vendor = "apple")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_to_patch_preserves_old_file_and_finishes_after_explicit_review() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), "old\n").expect("fixture");
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let edit = j
        .authorize_file_edit("alice", "edit", "policy-v1", f.workspace.path())
        .expect("edit");
    let auth = AgentAuthority::read(&f.grant)
        .with_memory(&grants)
        .with_editing(&edit);
    let model = ScriptedModel::new(vec![
        memory_call(),
        tools(vec![call(
            "patch",
            "apply_patch",
            json!({"path":"note.txt","expected_sha256":hex::encode(Sha256::digest("old\n")),"patch":"@@ -1 +1 @@\n-old\n+remembered bytes\n"}),
        )]),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    read_memory_step(&mut j, &f, auth, &model, rx.clone()).await;
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("edit proposal");
    let id = &v.pending[0].operation_id;
    let preview = j
        .agent_patch_preview_authorized("alice", "run-1", id, auth)
        .await
        .expect("exact patch");
    j.approve_agent_editing_authorized(
        "alice",
        "run-1",
        EditingApproval {
            operation_id: id,
            preview_digest: &preview.preview_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("edit");
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("note.txt")).expect("result"),
        "remembered bytes\n"
    );
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("finish")
            .phase,
        Phase::Completed
    );
}
#[cfg(target_vendor = "apple")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_scope_revoke_waits_for_real_patch_publication_to_leave_its_critical_section() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), "old\n").expect("fixture");
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let edit = j
        .authorize_file_edit("alice", "edit", "policy-v1", f.workspace.path())
        .expect("edit");
    let auth = AgentAuthority::read(&f.grant)
        .with_memory(&grants)
        .with_editing(&edit);
    let model = ScriptedModel::new(vec![
        memory_call(),
        tools(vec![call(
            "patch",
            "apply_patch",
            json!({"path":"note.txt","expected_sha256":hex::encode(Sha256::digest("old\n")),"patch":"@@ -1 +1 @@\n-old\n+remembered bytes\n"}),
        )]),
    ]);
    let (_tx, rx) = watch::channel(false);
    read_memory_step(&mut j, &f, auth, &model, rx.clone()).await;
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("proposal");
    let id = &v.pending[0].operation_id;
    let p = j
        .agent_patch_preview_authorized("alice", "run-1", id, auth)
        .await
        .expect("preview");
    j.approve_agent_editing_authorized(
        "alice",
        "run-1",
        EditingApproval {
            operation_id: id,
            preview_digest: &p.preview_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve");
    let stored = j.stored_run("alice", "run-1").expect("stored");
    let op = j.operation_record(&stored.config, id).expect("operation");
    let revoke = j
        .memory_scope_revoker("alice", "personal", "1")
        .expect("owner revoker");
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let seen = entered.clone();
    let release = gate.clone();
    let control = tokio::spawn(async move {
        seen.notified().await;
        revoke.request_revoke();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), revoke.wait_drained())
                .await
                .is_err(),
            "publication still owns dependency leases"
        );
        {
            let (lock, cond) = &*release;
            *lock.lock().expect("release") = true;
            cond.notify_all();
        }
        tokio::time::timeout(Duration::from_secs(2), revoke.wait_drained())
            .await
            .expect("finished publication");
    });
    j.advance_edit_tool_with_hook(&stored, &op, auth, rx, move |stage| {
        if stage == 2 {
            entered.notify_one();
            let (lock, cond) = &*gate;
            let mut ready = lock.lock().expect("gate");
            while !*ready {
                ready = cond.wait(ready).expect("wait");
            }
        }
        Ok(())
    })
    .await
    .expect("controlled stop");
    control.await.expect("control");
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("note.txt")).expect("original kept"),
        "old\n"
    );
    assert_ne!(
        j.state("alice", id).expect("state").as_deref(),
        Some("succeeded")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_workspace_consent_drains_a_dependent_worker_using_a_different_reader_grant() {
    let mut f = Fixture::new();
    let mut j = f.open();
    let source = j
        .authorize_workspace("alice", "source", f.workspace.path(), "policy-v1")
        .expect("source consent");
    f.grant = j
        .open_workspace("alice", &source.workspace_id)
        .expect("source");
    f.config.environment = f.grant.environment().into();
    let grants = vec![seed(&mut j)];
    let entry = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let output = tempfile::tempdir().expect("output");
    let read = WorkspaceGrant::open(
        "alice",
        "output-read",
        "policy-v1",
        output.path(),
        &[f.private.path().into()],
    )
    .expect("output read");
    let create = j
        .authorize_file_creation("alice", "output-write", "policy-v1", output.path())
        .expect("output create");
    let sources = vec![
        WorkspaceGrant::open(
            "alice",
            "independent-source-reader",
            "policy-v1",
            f.workspace.path(),
            &[f.private.path().into()],
        )
        .expect("separate source access"),
    ];
    let auth = AgentAuthority::read(&read)
        .with_creation(&create)
        .with_memory(&grants)
        .with_source_workspaces(&sources);
    let mut c = f.config.clone();
    c.run_id = "dependent-writer".into();
    c.environment = read.environment().into();
    let model = ScriptedModel::new(vec![
        tools(vec![call(
            "derived",
            "memory_read",
            json!({"scope_id":"personal","entry_id":entry}),
        )]),
        create_call(),
    ]);
    let (_tx, rx) = watch::channel(false);
    j.start_agent_authorized(
        &c,
        vec![Message::plain(
            Role::User,
            "Write from the derived memory.".into(),
        )],
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
    .expect("read approval");
    j.advance_agent_authorized("alice", &c.run_id, auth, &model, rx.clone())
        .await
        .expect("read with proof");
    let v = j
        .advance_agent_authorized("alice", &c.run_id, auth, &model, rx)
        .await
        .expect("write intent");
    let id = &v.pending[0].operation_id;
    j.approve_agent_creation_authorized(
        "alice",
        &c.run_id,
        CreationApproval {
            operation_id: id,
            argument_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("write approval");
    let stored = j.stored_run("alice", &c.run_id).expect("stored");
    let op = j.operation_record(&stored.config, id).expect("op");
    let fence = j
        .revalidate_sources_authority(&stored.config, auth)
        .await
        .expect("all source leases")
        .into_effect_guard();
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let notify = entered.clone();
    let paused = gate.clone();
    fence.before_for_test("create.before_publish", move || {
        notify.notify_one();
        let (lock, cond) = &*paused;
        let mut ready = lock.lock().expect("gate");
        while !*ready {
            ready = cond.wait(ready).expect("wait");
        }
    });
    let revoke = j
        .workspace_revoker("alice", &source.workspace_id)
        .expect("original consent revoker");
    let operation = op.operation(&stored.config);
    {
        let mut work = Box::pin(j.create_file_for_agent_with_sources(
            &create,
            &operation,
            CreateFileArgs {
                path: "from-memory.txt".into(),
                content: "remembered bytes".into(),
            },
            &read,
            Some(fence),
        ));
        tokio::select! {_=entered.notified()=>{},value=&mut work=>panic!("unexpected completion {value:?}")};
        revoke.request_revoke();
        assert!(
            !sources[0].is_revoked(),
            "the independent current reader has a different session signal"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), revoke.wait_drained())
                .await
                .is_err(),
            "original consent tracks indirect consumers too"
        );
        {
            let (lock, cond) = &*gate;
            *lock.lock().expect("release") = true;
            cond.notify_all();
        }
        assert!(work.await.is_err());
    }
    tokio::time::timeout(Duration::from_secs(2), revoke.wait_drained())
        .await
        .expect("actual indirect worker drained");
    assert!(!output.path().join("from-memory.txt").exists());
}
#[tokio::test]
async fn current_revision_revoker_still_waits_for_prior_revision_receipts() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = seed(&mut j);
    let receipt = j
        .read_memory(&grant, "preference", None, 0, 4096, false)
        .expect("held old revision");
    j.set_memory_scope_enabled("alice", "personal", "1", false)
        .expect("durable scope mutation");
    let revoker = j
        .memory_scope_revoker("alice", "personal", "2")
        .expect("current revision handle");
    revoker.request_revoke();
    assert!(receipt.view(&j).is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), revoker.wait_drained())
            .await
            .is_err(),
        "a new scope revision must not hide an old active lease"
    );
    drop(receipt);
    tokio::time::timeout(Duration::from_secs(1), revoker.wait_drained())
        .await
        .expect("drained");
    let view = j
        .authorize_workspace("alice", "source", f.workspace.path(), "policy-v1")
        .expect("workspace");
    let source = j
        .open_workspace("alice", &view.workspace_id)
        .expect("grant");
    let held = source
        .execute(
            "alice",
            source.environment(),
            "policy-v1",
            WorkspaceRequest::parse("read_file", &json!({"path":"note.txt"})).expect("request"),
        )
        .await
        .expect("held source read");
    j.revoke_workspace("alice", &view.workspace_id, &view.version)
        .expect("durable revoke");
    let revoked = j
        .workspace_revoker("alice", &view.workspace_id)
        .expect("revoker does not restore data permission");
    assert!(j.open_workspace("alice", &view.workspace_id).is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), revoked.wait_drained())
            .await
            .is_err()
    );
    drop(held);
    tokio::time::timeout(Duration::from_secs(1), revoked.wait_drained())
        .await
        .expect("source drained");
}
#[test]
fn unknown_effect_lifetime_policy_is_rejected_before_rewriting_a_journal() {
    let f = Fixture::new();
    let j = f.open();
    j.connection.execute_batch("DROP TRIGGER effect_policy_no_update;PRAGMA ignore_check_constraints=ON;UPDATE runtime_effect_policy SET policy='future-lifetime.v2';").expect("owned future-policy fixture");
    drop(j);
    let path = f.private.path().join("journal.sqlite");
    let before = std::fs::read(&path).expect("bytes");
    assert!(matches!(
        Journal::open(&path),
        Err(JournalError::UnsupportedVersion)
    ));
    assert_eq!(std::fs::read(path).expect("unchanged"), before);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_learned_after_file_creation_tracks_the_actual_published_version() {
    let f = Fixture::new();
    let mut j = f.open();
    let grants = vec![seed(&mut j)];
    let create = j
        .authorize_file_creation("alice", "create", "policy-v1", f.workspace.path())
        .expect("create");
    let auth = AgentAuthority::read(&f.grant)
        .with_memory(&grants)
        .with_creation(&create);
    let proposal = tools(vec![call(
        "learn-output",
        "memory_write",
        json!({"scope_id":"personal","mutation":"put","payload":{"title":"Output record","content":"The file contains remembered bytes.","enabled":true}}),
    )]);
    let model = ScriptedModel::new(vec![
        memory_call(),
        create_call(),
        proposal,
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    read_memory_step(&mut j, &f, auth, &model, rx.clone()).await;
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("create proposal");
    j.approve_agent_creation_authorized(
        "alice",
        "run-1",
        CreationApproval {
            operation_id: &v.pending[0].operation_id,
            argument_digest: &v.pending[0].argument_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve create");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("create");
    let v = j
        .advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("learning proposal");
    let preview = j
        .agent_memory_preview("alice", "run-1", &v.pending[0].operation_id, auth)
        .await
        .expect("published file has supported provenance");
    assert!(
        preview
            .provenance
            .sources
            .iter()
            .any(|s| matches!(s, MemorySource::Workspace { .. }))
    );
    j.approve_agent_memory(
        "alice",
        "run-1",
        AgentMemoryApproval {
            operation_id: &preview.operation_id,
            preview_digest: &preview.approval_digest,
            expires_at: now().expect("time") + 60,
        },
        auth,
    )
    .await
    .expect("approve provenance");
    j.advance_agent_authorized("alice", "run-1", auth, &model, rx.clone())
        .await
        .expect("learn");
    assert_eq!(
        j.advance_agent_authorized("alice", "run-1", auth, &model, rx)
            .await
            .expect("finish")
            .phase,
        Phase::Completed
    );
    std::fs::write(
        f.workspace.path().join("from-memory.txt"),
        "external replacement",
    )
    .expect("owned external writer");
    assert!(
        j.agent_view_authorized_scopes("alice", "run-1", auth)
            .await
            .is_err()
    );
}
