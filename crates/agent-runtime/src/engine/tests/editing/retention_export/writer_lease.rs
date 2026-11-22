use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_owner_cannot_certify_quiescence_while_its_detached_writer_is_alive() {
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
    let output = tempfile::tempdir().expect("destination");
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
            "paused",
        )
        .await
        .expect("preview");
    let review = prepared.preview().expect("review").clone();
    let (staged_tx, staged_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    prepared.on_staged_for_test(move || {
        let _ = staged_tx.send(());
        let _ = resume_rx.recv_timeout(Duration::from_secs(10));
    });
    let digest = review.digest.clone();
    let task = tokio::spawn(async move {
        j.approve_retention_export(&source, &destination, prepared, &digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), staged_rx)
        .await
        .expect("actual stage reached")
        .expect("stage signal");
    task.abort();
    assert!(task.await.expect_err("owner cancelled").is_cancelled());
    let reopened = f.open();
    let reader = reopened
        .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
        .expect("fresh reader after Journal reopened");
    assert_eq!(
        reopened
            .retention_export_outcome("alice", "run-1", &op, &review.export_id)
            .expect("outcome")
            .expect("unknown")
            .state,
        "unknown"
    );
    assert!(
        reopened.acquire_export_quiescence(&reader).await.is_err(),
        "a closed Journal and matching staging file do not prove a live writer ended"
    );
    let snapshot = reopened
        .inspect_export_files(&reader)
        .await
        .expect("readonly snapshot");
    assert!(matches!(
        snapshot.view().expect("view").staged,
        ExportFileObservation::Regular {
            matches_approved_artifact: true,
            ..
        }
    ));
    drop(snapshot);
    resume_tx.send(()).expect("release owned writer hook");
    let proof = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(proof) = reopened.acquire_export_quiescence(&reader).await {
                break proof;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("writer releases original lease after stopping");
    assert_eq!(proof.export_id().expect("bound proof"), review.export_id);
    assert!(
        reopened.acquire_export_quiescence(&reader).await.is_err(),
        "another inspector cannot borrow an already-held quiescence proof"
    );
    assert!(!output.path().join(&review.file_name).exists());
    reader.request_revoke();
    assert!(proof.check().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reader.wait_drained())
            .await
            .is_err()
    );
    drop(proof);
    tokio::time::timeout(Duration::from_secs(1), reader.wait_drained())
        .await
        .expect("proof read lease drained");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_identity_replacement_is_refused_and_legacy_records_are_not_certified() {
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
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
            "one",
        )
        .await
        .expect("preview");
    let review = prepared.preview().expect("review").clone();
    j.approve_retention_export(&source, &destination, prepared, &review.digest)
        .await
        .expect("publication");
    let reader = j
        .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
        .expect("read");
    let proof = j
        .acquire_export_quiescence(&reader)
        .await
        .expect("writer complete");
    let lease = f
        .private
        .path()
        .join(format!(".export-writer-{}.lease", review.export_id));
    let moved = f.private.path().join("owned-old-lease");
    std::fs::rename(&lease, &moved).expect("owned path replacement");
    std::fs::copy(&moved, &lease).expect("same bytes, different inode");
    std::fs::set_permissions(&lease, std::fs::Permissions::from_mode(0o600)).expect("mode");
    assert!(proof.check().is_err());
    assert!(j.acquire_export_quiescence(&reader).await.is_err());
    std::fs::remove_file(&lease).expect("owned replacement only");
    std::fs::rename(&moved, &lease).expect("restore original inode");
    proof.check().expect("original binding restored");
    drop(proof);
    assert!(
        j.connection
            .execute(
                "DELETE FROM export_writer_bindings WHERE export_id=?",
                [&review.export_id]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "UPDATE export_writer_bindings SET digest=? WHERE export_id=?",
                params!["0".repeat(64), review.export_id]
            )
            .is_err()
    );
    // Explicit prior schema shape: v18 never contained writer bindings.
    j.connection
        .execute_batch("DROP TABLE runtime_model_observations; DROP VIEW available_skill_versions; DROP TABLE local_run_skills; DROP TABLE local_skill_selections; DROP TABLE local_skill_packages; DROP TABLE runtime_skill_bindings; DROP TABLE owner_skills; DROP TABLE skill_versions; DROP TABLE memory_owner_removals; DROP TABLE runtime_memory_continuity_policy; DROP INDEX memory_approved_version; DROP TABLE runtime_memory_protocol; DROP TABLE runtime_effect_policy; DROP TABLE runtime_memory_lineage; DROP TABLE runtime_workspace_roots; DROP TABLE runtime_memory_sources; DROP TABLE runtime_memory_operations; DROP TABLE runtime_memory_capabilities; DROP TABLE memory_source_blocks; DROP TABLE memory_audit; DROP TABLE memory_drafts; DROP TABLE memory_entries; DROP TABLE memory_members; DROP TABLE memory_scopes; DROP TABLE export_restorations; DROP TABLE export_retirement_capsules; DROP TABLE export_retirements; DROP TABLE export_writer_bindings; PRAGMA user_version=18;")
        .expect("legacy fixture");
    drop(j);
    let j = f.open();
    let reader = j
        .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
        .expect("legacy remains readable");
    assert!(j.inspect_export_files(&reader).await.is_ok());
    assert!(
        j.acquire_export_quiescence(&reader).await.is_err(),
        "migration must not invent proof for a legacy worker"
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("source unchanged"),
        b"new\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_writer_lock_is_exclusive_across_processes() {
    use std::io::Read;
    if let (Ok(path), Ok(ready)) = (
        std::env::var("ORANGE_OWNED_WRITER_LEASE"),
        std::env::var("ORANGE_OWNED_WRITER_READY"),
    ) {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("owned child lease");
        file.try_lock().expect("child owns lease");
        std::fs::write(ready, b"held").expect("ready");
        let mut byte = [0];
        let _ = std::io::stdin().read(&mut byte);
        drop(file);
        return;
    }
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
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
            "one",
        )
        .await
        .expect("preview");
    let review = prepared.preview().expect("review").clone();
    j.approve_retention_export(&source, &destination, prepared, &review.digest)
        .await
        .expect("export");
    let lease = f
        .private
        .path()
        .join(format!(".export-writer-{}.lease", review.export_id));
    let ready = f.private.path().join("child-ready");
    let mut child=std::process::Command::new(std::env::current_exe().expect("test exe")).args(["engine::tests::editing::retention_export::writer_lease::original_writer_lock_is_exclusive_across_processes","--exact","--nocapture"]).env("ORANGE_OWNED_WRITER_LEASE",&lease).env("ORANGE_OWNED_WRITER_READY",&ready).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().expect("owned child");
    let reached = tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !reached {
        let _ = child.kill();
        let _ = child.wait();
        panic!("owned lease child failed to become ready");
    }
    let reader = j
        .authorize_export_inspection("alice", "run-1", &op, &review.export_id, output.path())
        .expect("read");
    let blocked = j.acquire_export_quiescence(&reader).await.is_err();
    drop(child.stdin.take());
    let exited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.try_wait().expect("owned status") {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if exited.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("owned lease child failed to exit");
    }
    assert!(exited.expect("status").success());
    assert!(
        blocked,
        "another process holding the original inode prevents certification"
    );
    j.acquire_export_quiescence(&reader)
        .await
        .expect("only after owned child exited");
}
