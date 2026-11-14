use super::*;
use crate::workspace::patch::review::dispatch::recovery::retention::{
    RetentionExportChoice, verify_retention_export,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_selection_is_independent_exact_revocable_and_epoch_bound() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("note.txt"), b"old\n").expect("source");
    let mut j = f.open();
    let edit = edit(&mut j, &f);
    j.start_agent_editing(
        &f.config,
        vec![Message::plain(Role::User, "Edit twice".into())],
        authority(&f, &edit),
        false,
    )
    .expect("start");
    let model = ScriptedModel::new(vec![
        patch_call("first", "note.txt", "old", "middle"),
        patch_call("second", "note.txt", "middle", "new"),
        final_response(),
    ]);
    let (_tx, rx) = watch::channel(false);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let pending = advance(&mut j, &f, &edit, &model, rx.clone()).await;
        ids.push(pending.pending[0].operation_id.clone());
        approve_patch(&mut j, &f, &edit, &pending).await;
        advance(&mut j, &f, &edit, &model, rx.clone()).await;
    }
    assert_eq!(
        advance(&mut j, &f, &edit, &model, rx).await.phase,
        Phase::Completed
    );
    f.grant.request_revoke();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        j.agent_retention_patch_ids("alice", "run-1", None, 100)
            .expect("owned index"),
        sorted
    );
    assert_eq!(
        j.agent_retention_patch_ids("alice", "run-1", Some(&sorted[0]), 1)
            .expect("next page"),
        sorted[1..]
    );
    assert!(
        j.agent_retention_patch_ids("alice", "run-1", Some(&sorted[1]), 1)
            .expect("end")
            .is_empty()
    );
    for (owner, run, cursor, limit) in [
        ("bob", "run-1", None, 1),
        ("alice", "other-run", None, 1),
        ("alice", "run-1", None, 0),
        ("alice", "run-1", None, 101),
        ("alice", "run-1", Some("invalid cursor"), 1),
    ] {
        assert!(
            j.agent_retention_patch_ids(owner, run, cursor, limit)
                .is_err()
        );
    }
    assert!(
        j.agent_retention_review_id("bob", "run-1", &ids[0])
            .is_err()
    );
    assert!(
        j.agent_retention_review_id("alice", "other-run", &ids[0])
            .is_err()
    );
    for (owner, run, operation, root) in [
        ("bob", "run-1", ids[0].as_str(), f.workspace.path()),
        ("alice", "other-run", ids[0].as_str(), f.workspace.path()),
        ("alice", "run-1", "other-operation", f.workspace.path()),
        ("alice", "run-1", ids[0].as_str(), f.private.path()),
    ] {
        assert!(
            j.authorize_patch_retention(owner, run, operation, root)
                .is_err()
        );
    }
    let elsewhere = tempfile::tempdir().expect("different directory");
    assert!(
        j.authorize_patch_retention("alice", "run-1", &ids[0], elsewhere.path())
            .is_err()
    );
    let selected = j
        .authorize_patch_retention("alice", "run-1", &ids[0], f.workspace.path())
        .expect("independent read choice after normal revocation");
    let other = j
        .authorize_patch_retention("alice", "run-1", &ids[1], f.workspace.path())
        .expect("separate operation");
    let export = j
        .capture_retention_export(&selected, RetentionExportChoice::Both)
        .await
        .expect("bounded original capture");
    let export_preview = export.preview().expect("preview");
    assert_eq!(export_preview.entries.len(), 1);
    assert_eq!(export_preview.entries[0].role, "original");
    assert!(export_preview.entries[0].matches_reviewed_original);
    assert!(export_preview.entries[0].reviewed_original_inode);
    let encoded = export.encoded_bytes().expect("captured bytes");
    assert_eq!(
        export_preview.artifact_sha256,
        hex::encode(Sha256::digest(encoded))
    );
    assert_eq!(export_preview.artifact_bytes, encoded.len().to_string());
    let document: serde_json::Value = serde_json::from_slice(encoded).expect("portable document");
    assert_eq!(
        &verify_retention_export(encoded).expect("portable integrity verifier"),
        export_preview
    );
    for corruption in [
        "content",
        "duplicate",
        "role",
        "bytes",
        "hash",
        "claim",
        "format",
        "unknown",
        "path",
    ] {
        let mut tampered = document.clone();
        match corruption {
            "content" => tampered["entries"][0]["content_base64"] = json!("Y2hhbmdlZA=="),
            "duplicate" => {
                let entry = tampered["entries"][0].clone();
                tampered["entries"]
                    .as_array_mut()
                    .expect("entries")
                    .push(entry);
            }
            "role" => tampered["entries"][0]["summary"]["role"] = json!("target"),
            "bytes" => tampered["entries"][0]["summary"]["bytes"] = json!("04"),
            "hash" => tampered["entries"][0]["summary"]["sha256"] = json!("0".repeat(64)),
            "claim" => {
                tampered["entries"][0]["summary"]["matches_reviewed_original"] = json!(false)
            }
            "format" => tampered["format"] = json!("orange-retained-files.v999"),
            "unknown" => tampered["execute_after_import"] = json!(true),
            "path" => tampered["source_path"] = json!("../escape"),
            _ => unreachable!(),
        }
        assert!(
            verify_retention_export(&serde_json::to_vec(&tampered).expect("tampered fixture"))
                .is_err(),
            "reject {corruption}"
        );
    }

    assert_eq!(document["format"], "orange-retained-files.v1");
    assert_eq!(document["source_path"], "note.txt");
    assert_eq!(
        STANDARD
            .decode(
                document["entries"][0]["content_base64"]
                    .as_str()
                    .expect("encoded content")
            )
            .expect("binary safe decode"),
        b"old\n"
    );
    assert!(!String::from_utf8_lossy(encoded).contains(".orange-agent-tmp-"));
    assert!(!String::from_utf8_lossy(encoded).contains(&f.private.path().display().to_string()));
    assert!(
        j.capture_retention_export(&selected, RetentionExportChoice::Staged)
            .await
            .is_err(),
        "missing staging file does not produce a fake export"
    );
    let again = j
        .capture_retention_export(&selected, RetentionExportChoice::Both)
        .await
        .expect("same actual snapshot");
    assert_eq!(again.encoded_bytes().expect("same payload"), encoded);
    drop(again);
    assert_eq!(
        j.selected_retention_usage(&selected)
            .expect("usage")
            .resource_active_records,
        2
    );
    let review = j
        .prepare_selected_retention(&selected, "first-review")
        .await
        .expect("preview");
    assert!(
        j.patch_retention_archive("alice", &ids[0])
            .expect("no automatic archival")
            .is_none()
    );
    assert!(
        j.approve_selected_retention(&other, &review.review_id, &review.digest)
            .await
            .is_err()
    );
    assert!(
        j.approve_selected_retention(&selected, &review.review_id, &"0".repeat(64))
            .await
            .is_err()
    );
    let receipt = j
        .inspect_selected_retention(&selected)
        .await
        .expect("snapshot");
    selected.request_revoke();
    assert!(receipt.snapshot().is_err());
    assert!(export.preview().is_err());
    assert!(export.encoded_bytes().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), selected.wait_drained())
            .await
            .is_err()
    );
    drop(receipt);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), selected.wait_drained())
            .await
            .is_err(),
        "export capture retains the real read lease"
    );
    drop(export);
    tokio::time::timeout(Duration::from_secs(1), selected.wait_drained())
        .await
        .expect("drained");
    assert!(j.selected_retention_usage(&selected).is_err());
    assert!(
        j.approve_selected_retention(&selected, &review.review_id, &review.digest)
            .await
            .is_err()
    );
    let selected = j
        .authorize_patch_retention("alice", "run-1", &ids[0], f.workspace.path())
        .expect("fresh choice");
    assert!(
        j.approve_selected_retention(&selected, &review.review_id, &review.digest)
            .await
            .is_err(),
        "new selection cannot revive old preview"
    );
    let review = j
        .prepare_selected_retention(&selected, "fresh-review")
        .await
        .expect("fresh exact preview");
    let archive = j
        .approve_selected_retention(&selected, &review.review_id, &review.digest)
        .await
        .expect("explicit archive");
    assert_eq!(
        j.agent_retention_review_id("alice", "run-1", &ids[0])
            .expect("lost reply lookup"),
        Some(review.review_id.clone())
    );
    assert_eq!(
        j.approve_selected_retention(&selected, &review.review_id, &review.digest)
            .await
            .expect("same review retry"),
        archive
    );
    let usage = j
        .selected_retention_usage(&selected)
        .expect("once-only accounting");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (1, 1)
    );
    let archived = j
        .inspect_selected_archive(&selected)
        .await
        .expect("live archive");
    assert!(!archived.view().expect("snapshot").changed_since_archival);
    selected.request_revoke();
    assert!(archived.view().is_err());
    drop(archived);
    let prior = j
        .prepare_selected_retention(&other, "before-reopen")
        .await
        .expect("old epoch preview");
    drop(j);
    let mut j = f.open();
    assert!(j.selected_retention_usage(&other).is_err());
    assert!(
        j.approve_selected_retention(&other, &prior.review_id, &prior.digest)
            .await
            .is_err()
    );
    let fresh = j
        .authorize_patch_retention("alice", "run-1", &ids[0], f.workspace.path())
        .expect("archived files remain inspectable after reopen");
    assert_eq!(
        j.inspect_selected_archive(&fresh)
            .await
            .expect("reopened archive")
            .view()
            .expect("live")
            .archive,
        archive
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("actual target"),
        b"new\n"
    );
    assert_eq!(
        std::fs::read_dir(f.workspace.path())
            .expect("no deletion or export")
            .count(),
        3
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        3,
        "no normal Run replay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_captures_changed_retained_bytes_without_claiming_original_and_refuses_links() {
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
    let operation = pending.pending[0].operation_id.clone();
    approve_patch(&mut j, &f, &edit, &pending).await;
    advance(&mut j, &f, &edit, &model, rx.clone()).await;
    assert_eq!(
        advance(&mut j, &f, &edit, &model, rx).await.phase,
        Phase::Completed
    );
    let plan = j
        .patch_publication_plan("alice", &operation)
        .expect("plan")
        .expect("owned publication");
    let original = f.workspace.path().join(plan.backup_name);
    let selection = j
        .authorize_patch_retention("alice", "run-1", &operation, f.workspace.path())
        .expect("separate read selection");
    // External late writes to the retained inode are real; the archive must
    // export its observed bytes and label them as changed, not fabricate old data.
    let binary = [0, 255, 128, 10];
    std::fs::write(&original, binary).expect("external changed retained inode");
    let capture = j
        .capture_retention_export(&selection, RetentionExportChoice::Original)
        .await
        .expect("binary-safe capture");
    assert!(!capture.preview().expect("preview").entries[0].matches_reviewed_original);
    let value: serde_json::Value =
        serde_json::from_slice(capture.encoded_bytes().expect("bytes")).expect("document");
    assert_eq!(
        STANDARD
            .decode(
                value["entries"][0]["content_base64"]
                    .as_str()
                    .expect("encoded")
            )
            .expect("decode"),
        binary
    );
    drop(capture);
    let extra = f.workspace.path().join("extra-link");
    std::fs::hard_link(&original, &extra).expect("owned hard link");
    assert!(
        j.capture_retention_export(&selection, RetentionExportChoice::Both)
            .await
            .is_err()
    );
    std::fs::remove_file(&extra).expect("remove owned extra link");
    assert!(
        j.capture_retention_export(&selection, RetentionExportChoice::Original)
            .await
            .is_ok(),
        "refused file type is not implicit revocation"
    );
    std::fs::remove_file(&original).expect("owned external replacement");
    std::os::unix::fs::symlink(f.workspace.path().join("note.txt"), &original)
        .expect("owned symlink");
    assert!(
        j.capture_retention_export(&selection, RetentionExportChoice::Both)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(f.workspace.path().join("note.txt")).expect("target unchanged"),
        b"new\n"
    );
    assert!(
        j.agent_retention_review_id("alice", "run-1", &operation)
            .expect("capture has no archival effect")
            .is_none()
    );
}
