use super::*;
mod retirement;
mod writer_lease;
use crate::workspace::patch::review::dispatch::recovery::retention::{
    ExportFileObservation, RetentionExportChoice, verify_retention_export,
};
use std::sync::Arc;

async fn edited(f: &Fixture) -> (Journal, String) {
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("owned source");
    let mut j = f.open();
    let edit = edit(&mut j, f);
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit".into())],
        authority(f, &edit),
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![
        patch_call("edit", "note.txt", "old", "new"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let pending = advance(&mut j, f, &edit, &model, rx.clone()).await;
    let operation = pending.pending[0].operation_id.clone();
    approve_patch(&mut j, f, &edit, &pending).await;
    advance(&mut j, f, &edit, &model, rx.clone()).await;
    assert_eq!(
        advance(&mut j, f, &edit, &model, rx).await.phase,
        Phase::Completed
    );
    (j, operation)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_export_requires_exact_approval_creates_verified_artifact_once_and_preserves_source()
{
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
    let output = tempfile::tempdir().expect("explicit destination");
    let source = j
        .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
        .expect("read scope");
    let destination = j
        .authorize_retention_export_destination(&source, output.path())
        .expect("separate destination");
    let first = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "proof.orange-retained.json",
            "one",
        )
        .await
        .expect("preview");
    let review = first.preview().expect("live review").clone();
    assert_eq!(
        std::fs::read_dir(output.path())
            .expect("no preview effects")
            .count(),
        0
    );
    assert!(
        j.approve_retention_export(&source, &destination, first, &"0".repeat(64))
            .await
            .is_err()
    );
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("metadata")
            .expect("known")
            .state,
        "prepared"
    );
    let approved = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "proof.orange-retained.json",
            "one",
        )
        .await
        .expect("same explicit review");
    assert_eq!(approved.preview().expect("same digest"), &review);
    let duplicate = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "proof.orange-retained.json",
            "one",
        )
        .await
        .expect("duplicate before dispatch");
    let receipt = j
        .approve_retention_export(&source, &destination, approved, &review.digest)
        .await
        .expect("actual publication and receipt");
    assert!(
        j.approve_retention_export(&source, &destination, duplicate, &review.digest)
            .await
            .is_err(),
        "immutable dispatch cannot replay"
    );
    let body = std::fs::read(output.path().join(&receipt.file_name)).expect("actual exported file");
    assert_eq!(
        verify_retention_export(&body).expect("offline integrity"),
        review.artifact
    );
    assert_eq!(receipt.sha256, hex::encode(Sha256::digest(&body)));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(output.path().join(&receipt.file_name))
            .expect("mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_dir(output.path())
            .expect("one final no temp")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(f.workspace.path())
            .expect("two source versions")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("target unchanged"),
        b"new\n"
    );
    assert!(
        j.agent_retention_review_id("alice", "run-1", &op)
            .expect("no implicit archive")
            .is_none()
    );
    assert!(
        j.retention_export_outcome("bob", "run-1", &op, &review.export_id)
            .expect("owner boundary")
            .is_none()
    );
    assert!(
        j.retention_export_outcome("alice", "other-run", &op, &review.export_id)
            .expect("run boundary")
            .is_none()
    );
    let other = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Both,
            "pending.orange-retained.json",
            "another-review",
        )
        .await
        .expect("another explicit preview");
    let other_id = other.preview().expect("id").export_id.clone();
    drop(other);
    let mut expected_ids = vec![review.export_id.clone(), other_id];
    expected_ids.sort_by(|a, b| b.cmp(a));
    let listed = j
        .retention_export_outcomes("alice", "run-1", &op, None, 100)
        .expect("owned export index");
    assert_eq!(
        listed
            .iter()
            .map(|r| r.export_id.clone())
            .collect::<Vec<_>>(),
        expected_ids
    );
    assert_eq!(
        j.retention_export_outcomes("alice", "run-1", &op, None, 1)
            .expect("first page")
            .len(),
        1
    );
    assert_eq!(
        j.retention_export_outcomes("alice", "run-1", &op, Some(&expected_ids[0]), 1)
            .expect("next page")[0]
            .export_id,
        expected_ids[1]
    );
    assert!(
        j.retention_export_outcomes("alice", "run-1", &op, Some(&expected_ids[1]), 1)
            .expect("end")
            .is_empty()
    );
    for (owner, run, cursor, limit) in [
        ("bob", "run-1", None, 1),
        ("alice", "other-run", None, 1),
        ("alice", "run-1", None, 0),
        ("alice", "run-1", None, 101),
        ("alice", "run-1", Some("bad cursor"), 1),
    ] {
        assert!(
            j.retention_export_outcomes(owner, run, &op, cursor, limit)
                .is_err()
        );
    }
    let encoded = serde_json::to_string(&listed).expect("metadata");
    assert!(
        !encoded.contains("proof.orange-retained.json")
            && !encoded.contains("content_base64")
            && !encoded.contains("note.txt")
    );
    source.request_revoke();
    destination.request_revoke();
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("lost response lookup after revoke")
            .expect("receipt")
            .state,
        "succeeded"
    );
    drop(j);
    let j = f.open();
    assert_eq!(
        j.retention_export_outcomes("alice", "run-1", &op, None, 100)
            .expect("discover after restart without file scopes"),
        listed
    );
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("reopen receipt")
            .expect("receipt")
            .state,
        "succeeded"
    );
    assert_eq!(
        std::fs::read_dir(output.path()).expect("no replay").count(),
        1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_conflict_revocation_and_lost_receipt_never_overwrite_or_replay() {
    for fault in [
        "target",
        "source-revoke",
        "destination-revoke",
        "dispatch-event",
        "receipt-event",
    ] {
        let f = Fixture::new();
        let (mut j, op) = edited(&f).await;
        let output = tempfile::tempdir().expect("destination");
        let source = j
            .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
            .expect("source");
        assert!(
            j.authorize_retention_export_destination(&source, f.private.path())
                .is_err()
        );
        let destination = j
            .authorize_retention_export_destination(&source, output.path())
            .expect("destination");
        assert!(
            j.prepare_retention_export(
                &source,
                &destination,
                RetentionExportChoice::Original,
                "../escape.orange-retained.json",
                "invalid"
            )
            .await
            .is_err()
        );
        let prepared = j
            .prepare_retention_export(
                &source,
                &destination,
                RetentionExportChoice::Original,
                "proof.orange-retained.json",
                "fault",
            )
            .await
            .expect("prepared");
        let review = prepared.preview().expect("review").clone();
        match fault {
            "target"=>std::fs::write(output.path().join(&review.file_name),b"external target").expect("late target"),
            "source-revoke"=>source.request_revoke(),
            "destination-revoke"=>destination.request_revoke(),
            "dispatch-event"=>j.connection.execute_batch("CREATE TRIGGER export_fault BEFORE INSERT ON events WHEN NEW.kind='patch.export_dispatched' BEGIN SELECT RAISE(ABORT,'owned before effect fault');END;").expect("fault"),
            "receipt-event"=>j.connection.execute_batch("CREATE TRIGGER export_fault BEFORE INSERT ON events WHEN NEW.kind='patch.export_completed' BEGIN SELECT RAISE(ABORT,'owned after effect fault');END;").expect("fault"),
            _=>unreachable!(),
        }
        assert!(
            j.approve_retention_export(&source, &destination, prepared, &review.digest)
                .await
                .is_err(),
            "{fault}"
        );
        let state = j
            .retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("outcome")
            .expect("record")
            .state;
        if ["target", "receipt-event"].contains(&fault) {
            assert_eq!(state, "unknown");
        } else {
            assert_eq!(state, "prepared");
        }
        if fault == "target" {
            assert_eq!(
                std::fs::read(output.path().join(&review.file_name)).expect("external untouched"),
                b"external target"
            );
        } else if fault == "receipt-event" {
            assert!(
                verify_retention_export(
                    &std::fs::read(output.path().join(&review.file_name))
                        .expect("effect exists but not success")
                )
                .is_ok()
            );
        } else {
            assert_eq!(
                std::fs::read_dir(output.path())
                    .expect("zero effects before dispatch")
                    .count(),
                0
            );
        }
        let count = std::fs::read_dir(output.path())
            .expect("before reopen")
            .count();
        assert!(
            j.authorize_export_inspection("bob", "run-1", &op, &review.export_id, output.path())
                .is_err()
        );
        assert!(
            j.authorize_export_inspection(
                "alice",
                "other-run",
                &op,
                &review.export_id,
                output.path()
            )
            .is_err()
        );
        assert!(
            j.authorize_export_inspection(
                "alice",
                "run-1",
                &op,
                &review.export_id,
                f.private.path()
            )
            .is_err()
        );
        assert!(
            j.authorize_export_inspection(
                "alice",
                "run-1",
                &op,
                &review.export_id,
                f.workspace.path()
            )
            .is_err()
        );
        let inspect = j
            .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
            .expect("independent destination read, even after source revoke");
        let observed = j
            .inspect_export_files(&inspect)
            .await
            .expect("actual current files");
        let view = observed.view().expect("live inspection");
        assert_eq!(view.recorded_state, state);
        match fault {
            "target" => {
                assert!(matches!(
                    view.target,
                    ExportFileObservation::Regular {
                        matches_approved_artifact: false,
                        valid_export_document: false,
                        ..
                    }
                ));
                assert!(matches!(
                    view.staged,
                    ExportFileObservation::Regular {
                        matches_approved_artifact: true,
                        valid_export_document: true,
                        ..
                    }
                ));
            }
            "receipt-event" => {
                assert!(matches!(
                    view.target,
                    ExportFileObservation::Regular {
                        matches_approved_artifact: true,
                        valid_export_document: true,
                        ..
                    }
                ));
                assert_eq!(view.staged, ExportFileObservation::Missing);
            }
            _ => {
                assert_eq!(view.target, ExportFileObservation::Missing);
                assert_eq!(view.staged, ExportFileObservation::Missing);
            }
        }
        let metadata = serde_json::to_string(view).expect("inspection projection");
        assert!(
            !metadata.contains("content_base64")
                && !metadata.contains(".orange-agent-tmp-export-")
                && !metadata.contains("note.txt")
        );
        inspect.request_revoke();
        assert!(observed.view().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inspect.wait_drained())
                .await
                .is_err()
        );
        drop(observed);
        tokio::time::timeout(Duration::from_secs(1), inspect.wait_drained())
            .await
            .expect("inspection drain");
        assert!(j.inspect_export_files(&inspect).await.is_err());
        let old_inspect = j
            .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
            .expect("scope before reopen");

        drop(j);
        let mut j = f.open();
        assert_eq!(
            j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
                .expect("reopen")
                .expect("record")
                .state,
            state
        );
        assert!(
            j.inspect_export_files(&old_inspect).await.is_err(),
            "old Host epoch cannot inspect"
        );
        let fresh_inspect = j
            .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
            .expect("fresh native read choice after reopen");
        let observed = j
            .inspect_export_files(&fresh_inspect)
            .await
            .expect("no source permission restoration");
        assert_eq!(observed.view().expect("snapshot").recorded_state, state);
        drop(observed);
        let fresh = j
            .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
            .expect("fresh read");
        let destination = j
            .authorize_retention_export_destination(&fresh, output.path())
            .expect("fresh destination");
        assert!(
            j.prepare_retention_export(
                &fresh,
                &destination,
                RetentionExportChoice::Original,
                "proof.orange-retained.json",
                "fault"
            )
            .await
            .is_err(),
            "old preview or unknown cannot become a new dispatch"
        );
        assert_eq!(
            std::fs::read_dir(output.path()).expect("no replay").count(),
            count
        );
        assert_eq!(
            std::fs::read(f.workspace.path().join("note.txt")).expect("source retained"),
            b"new\n"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_schema17_upgrade_and_interrupted_dispatch_are_nonreplaying() {
    let f = Fixture::new();
    let (j, op) = edited(&f).await;
    j.connection
        .execute_batch(
            "DROP TABLE runtime_memory_sources; DROP TABLE runtime_memory_operations; DROP TABLE runtime_memory_capabilities; DROP TABLE memory_source_blocks; DROP TABLE memory_audit; DROP TABLE memory_drafts; DROP TABLE memory_entries; DROP TABLE memory_members; DROP TABLE memory_scopes; DROP TABLE export_restorations; DROP TABLE export_retirement_capsules; DROP TABLE export_retirements; DROP TABLE export_writer_bindings; DROP TABLE patch_exports; PRAGMA user_version=17;",
        )
        .expect("true prior schema shape");
    drop(j);
    let mut j = f.open();
    let version: i64 = j
        .connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, 23);
    let output = tempfile::tempdir().expect("destination");
    let source = j
        .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
        .expect("source");
    let destination = j
        .authorize_retention_export_destination(&source, output.path())
        .expect("destination");
    let prepared = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "proof.orange-retained.json",
            "interrupted",
        )
        .await
        .expect("preview");
    let review = prepared.preview().expect("review").clone();
    drop(prepared);
    j.connection
        .execute(
            "UPDATE patch_exports SET state='dispatching' WHERE export_id=?",
            [&review.export_id],
        )
        .expect("owned crash after durable dispatch");
    assert!(
        j.connection
            .execute(
                "UPDATE patch_exports SET state='prepared' WHERE export_id=?",
                [&review.export_id]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "DELETE FROM patch_exports WHERE export_id=?",
                [&review.export_id]
            )
            .is_err()
    );
    drop(j);
    let j = f.open();
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("outcome")
            .expect("unknown")
            .state,
        "unknown"
    );
    assert_eq!(
        std::fs::read_dir(output.path()).expect("no replay").count(),
        0
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("source"),
        b"new\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_after_actual_export_staging_prevents_final_publication_and_replay() {
    for revoke_source in [true, false] {
        let f = Fixture::new();
        let (mut j, op) = edited(&f).await;
        let output = tempfile::tempdir().expect("owned destination");
        let source = Arc::new(
            j.authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
                .expect("source"),
        );
        let destination = Arc::new(
            j.authorize_retention_export_destination(&source, output.path())
                .expect("destination"),
        );
        let prepared = j
            .prepare_retention_export(
                &source,
                &destination,
                RetentionExportChoice::Original,
                "proof.orange-retained.json",
                "inflight",
            )
            .await
            .expect("preview");
        let review = prepared.preview().expect("review").clone();
        let callback_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = callback_ran.clone();
        let reader = source.clone();
        let writer = destination.clone();
        prepared.on_staged_for_test(move || {
            called.store(true, Ordering::SeqCst);
            if revoke_source {
                reader.request_revoke();
            } else {
                writer.request_revoke();
            }
        });
        assert!(
            j.approve_retention_export(&source, &destination, prepared, &review.digest)
                .await
                .is_err()
        );
        assert!(callback_ran.load(Ordering::SeqCst));
        assert!(!output.path().join(&review.file_name).exists());
        assert_eq!(
            std::fs::read_dir(output.path())
                .expect("preserved uncertain staging")
                .count(),
            1
        );
        assert_eq!(
            j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
                .expect("outcome")
                .expect("record")
                .state,
            "unknown"
        );
        tokio::time::timeout(Duration::from_secs(1), source.wait_drained())
            .await
            .expect("source worker ended");
        tokio::time::timeout(Duration::from_secs(1), destination.wait_drained())
            .await
            .expect("destination worker ended");
        drop(j);
        let j = f.open();
        assert_eq!(
            j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
                .expect("reopened outcome")
                .expect("record")
                .state,
            "unknown"
        );
        assert!(!output.path().join(&review.file_name).exists());
        assert_eq!(
            std::fs::read(f.workspace.path().join("note.txt")).expect("original target unchanged"),
            b"new\n"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_inspection_refuses_links_and_oversized_data_without_claiming_delivery() {
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
    let output = tempfile::tempdir().expect("destination");
    let source = j
        .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
        .expect("source");
    let destination = j
        .authorize_retention_export_destination(&source, output.path())
        .expect("write destination");
    let prepared = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "inspect.orange-retained.json",
            "one",
        )
        .await
        .expect("preview");
    let review = prepared.preview().expect("review").clone();
    j.approve_retention_export(&source, &destination, prepared, &review.digest)
        .await
        .expect("publish");
    source.request_revoke();
    destination.request_revoke();
    let selected = j
        .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
        .expect("separate reader");
    let target = output.path().join(&review.file_name);
    let link = output.path().join("extra");
    std::fs::hard_link(&target, &link).expect("external hard link");
    assert!(matches!(
        j.inspect_export_files(&selected)
            .await
            .expect("refuse hard link")
            .view()
            .expect("view")
            .target,
        ExportFileObservation::Refused { .. }
    ));
    std::fs::remove_file(&link).expect("owned link");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .expect("external writer")
        .set_len(3 * 1024 * 1024 + 1)
        .expect("oversized file");
    assert!(matches!(
        j.inspect_export_files(&selected)
            .await
            .expect("refuse size")
            .view()
            .expect("view")
            .target,
        ExportFileObservation::Refused { .. }
    ));
    std::fs::remove_file(&target).expect("owned external replacement");
    std::os::unix::fs::symlink(f.workspace.path().join("note.txt"), &target)
        .expect("external symlink");
    assert!(matches!(
        j.inspect_export_files(&selected)
            .await
            .expect("refuse link")
            .view()
            .expect("view")
            .target,
        ExportFileObservation::Refused { .. }
    ));
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("historical receipt")
            .expect("record")
            .state,
        "succeeded",
        "current file changes neither erase nor invent the immutable delivery receipt"
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("source not followed or altered"),
        b"new\n"
    );
}
