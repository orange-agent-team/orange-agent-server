use super::*;

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
    assert!(
        tokio::time::timeout(Duration::from_millis(20), selected.wait_drained())
            .await
            .is_err()
    );
    drop(receipt);
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
