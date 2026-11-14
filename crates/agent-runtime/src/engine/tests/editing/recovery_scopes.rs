use super::*;
use crate::workspace::patch::review::dispatch::recovery::{
    inspection::RecoveryFileObservation, resolution::PatchRecoveryChoice,
};

pub(super) async fn resolve(mut j: Journal, f: &Fixture, operation: &str) -> Journal {
    let unresolved = j
        .agent_patch_recovery_outcome("alice", "run-1", operation)
        .expect("unresolved metadata");
    assert!(unresolved.decision.is_none());
    assert_eq!(unresolved.original_execution_state, "unknown");
    assert!(
        j.agent_patch_recovery_outcome("bob", "run-1", operation)
            .is_err()
    );
    assert!(
        j.agent_patch_recovery_outcome("alice", "other-run", operation)
            .is_err()
    );
    assert!(
        j.agent_patch_recovery_outcome("alice", "run-1", "other-operation")
            .is_err()
    );
    assert!(
        j.authorize_patch_inspection("bob", "run-1", operation, f.workspace.path())
            .is_err()
    );
    assert!(
        j.authorize_patch_inspection("alice", "other-run", operation, f.workspace.path())
            .is_err()
    );
    assert!(
        j.authorize_patch_inspection("alice", "run-1", "other-operation", f.workspace.path())
            .is_err()
    );
    assert!(
        j.authorize_patch_inspection("alice", "run-1", operation, f.private.path())
            .is_err()
    );
    let elsewhere = tempfile::tempdir().expect("unrelated root");
    assert!(
        j.authorize_patch_inspection("alice", "run-1", operation, elsewhere.path())
            .is_err()
    );
    let first = j
        .authorize_patch_inspection("alice", "run-1", operation, f.workspace.path())
        .expect("fresh read selection");
    let receipt = j
        .inspect_selected_patch(&first)
        .await
        .expect("actual bounded snapshot");
    assert!(matches!(
        receipt.snapshot().expect("live snapshot").target,
        RecoveryFileObservation::Regular {
            matches_result_content: true,
            ..
        }
    ));
    first.request_revoke();
    assert!(
        receipt.snapshot().is_err(),
        "revoked receipt cannot reveal even hashes"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), first.wait_drained())
            .await
            .is_err()
    );
    drop(receipt);
    tokio::time::timeout(Duration::from_secs(1), first.wait_drained())
        .await
        .expect("release only after receipt drains");
    assert!(j.inspect_selected_patch(&first).await.is_err());
    assert!(
        j.authorize_selected_patch_recovery(&first, f.workspace.path())
            .is_err()
    );

    let read = j
        .authorize_patch_inspection("alice", "run-1", operation, f.workspace.path())
        .expect("new native read choice");
    assert!(
        j.authorize_selected_patch_recovery(&read, elsewhere.path())
            .is_err()
    );
    let write = j
        .authorize_selected_patch_recovery(&read, f.workspace.path())
        .expect("independent native write choice");
    let review = j
        .prepare_selected_patch_resolution(
            &read,
            &write,
            "selected-review",
            PatchRecoveryChoice::KeepCurrentTarget,
        )
        .await
        .expect("actual review");
    assert!(
        j.approve_selected_patch_resolution(
            &read,
            &write,
            &review.recovery_id,
            &"0".repeat(64),
            f.config.deadline_unix
        )
        .await
        .is_err()
    );
    assert!(
        j.dispatch_selected_patch_resolution(&read, &write, &review.recovery_id)
            .await
            .is_err(),
        "read/write consent is not operation approval"
    );
    let replacement = j
        .authorize_patch_inspection("alice", "run-1", operation, f.workspace.path())
        .expect("another explicit read selection");
    assert!(
        j.approve_selected_patch_resolution(
            &replacement,
            &write,
            &review.recovery_id,
            &review.digest,
            f.config.deadline_unix
        )
        .await
        .is_err(),
        "new scope cannot revive an old review"
    );
    j.approve_selected_patch_resolution(
        &read,
        &write,
        &review.recovery_id,
        &review.digest,
        f.config.deadline_unix,
    )
    .await
    .expect("exact approval");
    let token = j
        .dispatch_selected_patch_resolution(&read, &write, &review.recovery_id)
        .await
        .expect("durable recovery dispatch");
    read.request_revoke();
    assert!(
        j.publish_selected_patch_resolution(&read, &write, token)
            .is_err()
    );
    let state: String = j
        .connection
        .query_row(
            "SELECT state FROM patch_recoveries WHERE recovery_id=?",
            [&review.recovery_id],
            |r| r.get(0),
        )
        .expect("durable refused dispatch");
    assert_eq!(
        state, "unknown",
        "post-dispatch scope refusal must not leave a live dispatch marker"
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("unchanged effect"),
        b"new\n"
    );
    assert!(
        j.inspect_selected_patch(&replacement).await.is_ok(),
        "revoking one read scope does not revoke another"
    );

    drop(j);
    let mut j = f.open();
    assert!(
        j.inspect_selected_patch(&replacement).await.is_err(),
        "Host epoch requires fresh native authorization"
    );
    assert!(
        j.authorize_selected_patch_recovery(&replacement, f.workspace.path())
            .is_err()
    );
    let read = j
        .authorize_patch_inspection("alice", "run-1", operation, f.workspace.path())
        .expect("explicit read after reopen");
    let write = j
        .authorize_selected_patch_recovery(&read, f.workspace.path())
        .expect("explicit write after reopen");
    let review = j
        .prepare_selected_patch_resolution(
            &read,
            &write,
            "confirm-current",
            PatchRecoveryChoice::KeepCurrentTarget,
        )
        .await
        .expect("fresh review after refused recovery");
    j.approve_selected_patch_resolution(
        &read,
        &write,
        &review.recovery_id,
        &review.digest,
        f.config.deadline_unix,
    )
    .await
    .expect("new exact approval");
    let token = j
        .dispatch_selected_patch_resolution(&read, &write, &review.recovery_id)
        .await
        .expect("recovery dispatch");
    j.publish_selected_patch_resolution(&read, &write, token)
        .expect("keep actual current target");
    read.request_revoke();
    write.request_revoke();
    let outcome = j
        .agent_patch_recovery_outcome("alice", "run-1", operation)
        .expect("receipt despite revoked scopes");
    assert_eq!(outcome.original_execution_state, "unknown");
    assert_eq!(
        outcome
            .decision
            .as_ref()
            .expect("committed decision")
            .recovery_id,
        review.recovery_id
    );
    assert_eq!(
        outcome.decision.as_ref().expect("choice").choice,
        PatchRecoveryChoice::KeepCurrentTarget
    );
    let body = serde_json::to_value(&outcome).expect("metadata projection");
    assert_eq!(body.as_object().expect("object").len(), 4);
    assert_eq!(
        body["decision"]
            .as_object()
            .expect("decision metadata")
            .len(),
        2
    );
    assert!(!body.to_string().contains("sha256") && !body.to_string().contains("note.txt"));
    drop(j);
    let j = f.open();
    assert_eq!(
        j.agent_patch_recovery_outcome("alice", "run-1", operation)
            .expect("lost reply after reopen"),
        outcome
    );
    j
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_recovery_moves_only_to_missing_targets_and_never_replays_the_agent() {
    for choice in [
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
        PatchRecoveryChoice::PublishStagedToMissingTarget,
    ] {
        for late_target in [false, true] {
            let f = Fixture::new();
            let target = f.workspace.path().join("note.txt");
            std::fs::write(&target, b"old\n").expect("owned source");
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
            let operation = &pending.pending[0].operation_id;
            let stored = j.stored_run("alice", "run-1").expect("owned Run");
            let op = j
                .operation_record(&f.config, operation)
                .expect("owned operation");
            j.advance_edit_tool_with_hook(
                &stored,
                &op,
                authority(&f, &edit).internal(),
                rx,
                |stage| {
                    if stage == 3 {
                        return Err(crate::workspace::patch::PatchError::Invalid);
                    }
                    Ok(())
                },
            )
            .await
            .expect("interrupted after actual capture");
            assert!(!target.exists());
            let read = j
                .authorize_patch_inspection("alice", "run-1", operation, f.workspace.path())
                .expect("fresh inspection choice");
            let write = j
                .authorize_selected_patch_recovery(&read, f.workspace.path())
                .expect("separate write choice");
            let review = j
                .prepare_selected_patch_resolution(&read, &write, "explicit-resolution", choice)
                .await
                .expect("actual missing-target review");
            j.approve_selected_patch_resolution(
                &read,
                &write,
                &review.recovery_id,
                &review.digest,
                f.config.deadline_unix,
            )
            .await
            .expect("exact decision approval");
            let token = j
                .dispatch_selected_patch_resolution(&read, &write, &review.recovery_id)
                .await
                .expect("dispatch");
            if late_target {
                std::fs::write(&target, b"external").expect("owned racing target fixture");
            }
            let result = j.publish_selected_patch_resolution(&read, &write, token);
            if late_target {
                assert!(result.is_err());
                assert_eq!(
                    std::fs::read(&target).expect("racing target retained"),
                    b"external"
                );
                assert_eq!(
                    j.agent_unresolved_patch_ids("alice", "run-1", None, 10)
                        .expect("still unresolved"),
                    vec![operation.clone()]
                );
            } else {
                result.expect("selected recovery completed");
                let expected: &[u8] =
                    if choice == PatchRecoveryChoice::RestoreOriginalToMissingTarget {
                        b"old\n"
                    } else {
                        b"new\n"
                    };
                assert_eq!(std::fs::read(&target).expect("actual target"), expected);
                assert!(
                    j.agent_unresolved_patch_ids("alice", "run-1", None, 10)
                        .expect("resolved only")
                        .is_empty()
                );
                assert!(j.inspect_selected_patch(&read).await.is_ok());
            }
            assert_eq!(
                j.state("alice", operation)
                    .expect("original uncertainty retained")
                    .as_deref(),
                Some("unknown")
            );
            assert_eq!(
                j.agent_view("alice", "run-1")
                    .expect("Run remains stopped")
                    .phase,
                Phase::RecoveryRequired
            );
            assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        }
    }
}
