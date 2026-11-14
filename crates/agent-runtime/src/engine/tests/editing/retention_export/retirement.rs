use super::*;
use crate::workspace::patch::review::dispatch::recovery::retention::ExportRetirementChoice;
use std::{fs, path::Path};

async fn exported(f: &Fixture, output: &Path) -> (Journal, String, String, String) {
    let (mut j, op) = edited(f).await;
    let source = j
        .authorize_patch_retention("alice", "run-1", &op, f.workspace.path())
        .expect("source");
    let destination = j
        .authorize_retention_export_destination(&source, output)
        .expect("destination");
    let prepared = j
        .prepare_retention_export(
            &source,
            &destination,
            RetentionExportChoice::Original,
            "proof.orange-retained.json",
            "export",
        )
        .await
        .expect("preview");
    let preview = prepared.preview().expect("view").clone();
    j.approve_retention_export(&source, &destination, prepared, &preview.digest)
        .await
        .expect("export");
    (j, op, preview.export_id, preview.file_name)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_retirement_moves_to_retained_capsule_without_deletion_quota_release_or_replay() {
    let f = Fixture::new();
    let output = tempfile::tempdir().expect("output");
    let (mut j, op, id, name) = exported(&f, output.path()).await;
    let expected = fs::read(output.path().join(&name)).expect("exported bytes");
    let read = j
        .authorize_export_inspection("alice", "run-1", &op, &id, output.path())
        .expect("read choice");
    assert!(
        j.authorize_export_retirement(&read, f.workspace.path())
            .is_err()
    );
    assert!(
        j.authorize_export_retirement(&read, f.private.path())
            .is_err()
    );
    let write = j
        .authorize_export_retirement(&read, output.path())
        .expect("separate write choice");
    let prepared = j
        .prepare_export_retirement(&read, &write, ExportRetirementChoice::Target, "first")
        .await
        .expect("preview");
    let review = prepared.preview().expect("view").clone();
    assert_eq!(
        fs::read_dir(output.path())
            .expect("no prepare effects")
            .count(),
        1
    );
    assert!(
        j.approve_export_retirement(&read, &write, prepared, &"0".repeat(64))
            .await
            .is_err()
    );
    let prepared = j
        .prepare_export_retirement(&read, &write, ExportRetirementChoice::Target, "first")
        .await
        .expect("same review");
    assert_eq!(prepared.preview().expect("same"), &review);
    let receipt = j
        .approve_export_retirement(&read, &write, prepared, &review.digest)
        .await
        .expect("reversible capture");
    assert_eq!(receipt.disposition, "retained_in_capsule");
    assert!(!output.path().join(&name).exists());
    let capsule = output.path().join(format!(
        ".orange-agent-tmp-retired-{}",
        receipt.retirement_id
    ));
    assert_eq!(
        fs::read(capsule.join("item")).expect("data preserved"),
        expected
    );
    assert!(
        WorkspaceGrant::open(
            "alice",
            "forbidden-root",
            "policy-v1",
            &capsule,
            &[f.private.path().to_path_buf()]
        )
        .is_err(),
        "reserved capsule cannot become an ordinary model root"
    );
    assert!(
        j.prepare_export_retirement(&read, &write, ExportRetirementChoice::Target, "first")
            .await
            .is_err()
    );
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &id)
            .expect("old export")
            .expect("receipt")
            .state,
        "succeeded"
    );
    assert!(
        j.agent_retention_review_id("alice", "run-1", &op)
            .expect("no implicit archival budget action")
            .is_none()
    );
    assert!(
        j.export_retirement_outcome("bob", "run-1", &op, &receipt.retirement_id)
            .expect("owner boundary")
            .is_none()
    );
    read.request_revoke();
    write.request_revoke();
    let outcome = j
        .export_retirement_outcome("alice", "run-1", &op, &receipt.retirement_id)
        .expect("lost reply lookup")
        .expect("record");
    assert_eq!(outcome.state, "retained");
    assert_eq!(
        j.export_retirement_outcomes("alice", "run-1", &op, &id, None, 100)
            .expect("index"),
        vec![outcome.clone()]
    );
    assert!(
        j.export_retirement_outcomes(
            "alice",
            "run-1",
            &op,
            &id,
            Some(&receipt.retirement_id),
            100
        )
        .expect("cursor")
        .is_empty()
    );
    assert!(
        j.export_retirement_outcomes("bob", "run-1", &op, &id, None, 10)
            .is_err()
    );
    assert!(
        j.export_retirement_outcomes("alice", "run-1", &op, &id, None, 0)
            .is_err()
    );

    drop(j);
    let j = f.open();
    assert_eq!(
        j.export_retirement_outcome("alice", "run-1", &op, &receipt.retirement_id)
            .expect("reopen")
            .expect("record"),
        outcome
    );
    assert_eq!(
        fs::read(capsule.join("item")).expect("no replay or deletion"),
        expected
    );
    let fresh = j
        .authorize_export_inspection("alice", "run-1", &op, &id, output.path())
        .expect("fresh reader");
    let inspected = j
        .inspect_export_retirement(&fresh, &receipt.retirement_id)
        .await
        .expect("bound capsule read");
    assert!(inspected.view().expect("view").capsule_bound);
    assert!(matches!(
        inspected.view().expect("view").retained,
        Some(ExportFileObservation::Regular {
            matches_approved_artifact: true,
            ..
        })
    ));
    fresh.request_revoke();
    assert!(inspected.view().is_err());
    drop(inspected);

    assert_eq!(
        fs::read(f.workspace.path().join("note.txt")).expect("source unchanged"),
        b"new\n"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_handles_conflict_revocation_and_receipt_failure_without_erasing_data() {
    for fault in [
        "changed",
        "revoke",
        "capsule-exists",
        "receipt-failure",
        "late-replacement",
    ] {
        let f = Fixture::new();
        let output = tempfile::tempdir().expect("output");
        let (mut j, op, id, name) = exported(&f, output.path()).await;
        let expected = fs::read(output.path().join(&name)).expect("bytes");
        let read = Arc::new(
            j.authorize_export_inspection("alice", "run-1", &op, &id, output.path())
                .expect("read"),
        );
        let write = j
            .authorize_export_retirement(&read, output.path())
            .expect("write");
        let prepared = j
            .prepare_export_retirement(&read, &write, ExportRetirementChoice::Target, "fault")
            .await
            .expect("preview");
        let review = prepared.preview().expect("view").clone();
        let capsule = output.path().join(format!(
            ".orange-agent-tmp-retired-{}",
            review.retirement_id
        ));
        match fault {
            "changed"=>fs::write(output.path().join(&name),b"outside change").expect("external edit"),
            "revoke"=>{let read=read.clone();prepared.before_capture_for_test(move||read.request_revoke());},
            "capsule-exists"=>{fs::create_dir(&capsule).expect("collision");fs::write(capsule.join("keep"),b"external").expect("foreign data");},
            "receipt-failure"=>j.connection.execute_batch("CREATE TRIGGER retirement_fault BEFORE INSERT ON events WHEN NEW.kind='patch.export_retirement_retained' BEGIN SELECT RAISE(ABORT,'owned receipt fault');END;").expect("fault"),
            "late-replacement"=>{let path=output.path().join(&name);let moved=output.path().join("external-original");prepared.before_capture_for_test(move||{fs::rename(&path,moved).expect("external rename");fs::write(path,b"replacement").expect("external replacement");});},
            _=>unreachable!(),
        }
        assert!(
            j.approve_export_retirement(&read, &write, prepared, &review.digest)
                .await
                .is_err(),
            "{fault}"
        );
        let outcome = j
            .export_retirement_outcome("alice", "run-1", &op, &review.retirement_id)
            .expect("outcome")
            .expect("record");
        assert_eq!(
            outcome.state,
            if fault == "changed" {
                "prepared"
            } else {
                "unknown"
            }
        );
        match fault {
            "changed" => assert_eq!(
                fs::read(output.path().join(&name)).expect("kept"),
                b"outside change"
            ),
            "revoke" | "capsule-exists" => assert_eq!(
                fs::read(output.path().join(&name)).expect("no capture"),
                expected
            ),
            "receipt-failure" => assert_eq!(
                fs::read(capsule.join("item")).expect("retained despite missing receipt"),
                expected
            ),
            "late-replacement" => {
                assert_eq!(
                    fs::read(capsule.join("item")).expect("foreign capture preserved"),
                    b"replacement"
                );
                assert_eq!(
                    fs::read(output.path().join("external-original"))
                        .expect("reviewed object preserved"),
                    expected
                );
            }
            _ => unreachable!(),
        }
        if fault == "capsule-exists" {
            assert_eq!(
                fs::read(capsule.join("keep")).expect("not replaced"),
                b"external"
            );
        }
        let entries = fs::read_dir(output.path()).expect("before reopen").count();
        drop(j);
        let j = f.open();
        assert_eq!(
            j.export_retirement_outcome("alice", "run-1", &op, &review.retirement_id)
                .expect("reopen")
                .expect("record"),
            outcome
        );
        assert_eq!(
            fs::read_dir(output.path()).expect("no replay").count(),
            entries
        );
        let fresh = j
            .authorize_export_inspection("alice", "run-1", &op, &id, output.path())
            .expect("fresh read");
        let inspected = j
            .inspect_export_retirement(&fresh, &review.retirement_id)
            .await
            .expect("inspection is not resolution");
        assert_eq!(
            inspected.view().expect("view").recorded_state,
            outcome.state
        );
        if fault == "late-replacement" {
            assert!(matches!(
                inspected.view().expect("foreign retained view").retained,
                Some(ExportFileObservation::Regular {
                    matches_approved_artifact: false,
                    ..
                })
            ));
        }
        if fault == "receipt-failure" {
            assert!(matches!(
                inspected.view().expect("actual retained view").retained,
                Some(ExportFileObservation::Regular {
                    matches_approved_artifact: true,
                    ..
                })
            ));
        }
        if fault == "changed" || fault == "capsule-exists" {
            assert!(!inspected.view().expect("unbound").capsule_bound);
            assert!(inspected.view().expect("unbound").retained.is_none());
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_package_can_be_retired_while_conflicting_target_and_original_unknown_are_preserved()
{
    let f = Fixture::new();
    let (mut j, op) = edited(&f).await;
    let output = tempfile::tempdir().expect("output");
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
        .expect("export preview");
    let export = prepared.preview().expect("view").clone();
    fs::write(output.path().join(&export.file_name), b"foreign target").expect("late target");
    assert!(
        j.approve_retention_export(&source, &destination, prepared, &export.digest)
            .await
            .is_err()
    );
    let read = j
        .authorize_export_inspection("alice", "run-1", &op, &export.export_id, output.path())
        .expect("read");
    let write = j
        .authorize_export_retirement(&read, output.path())
        .expect("write");
    assert!(
        j.prepare_export_retirement(
            &read,
            &write,
            ExportRetirementChoice::Target,
            "do-not-move-foreign"
        )
        .await
        .is_err()
    );
    let prepared = j
        .prepare_export_retirement(
            &read,
            &write,
            ExportRetirementChoice::Staged,
            "retain-stage",
        )
        .await
        .expect("matching staging preview");
    let review = prepared.preview().expect("view").clone();
    j.approve_export_retirement(&read, &write, prepared, &review.digest)
        .await
        .expect("retain stage");
    assert_eq!(
        fs::read(output.path().join(&export.file_name)).expect("foreign untouched"),
        b"foreign target"
    );
    assert_eq!(
        j.retention_export_outcome("alice", "run-1", &op, &export.export_id)
            .expect("original outcome")
            .expect("record")
            .state,
        "unknown"
    );
    let saved = fs::read(output.path().join(format!(
        ".orange-agent-tmp-retired-{}/item",
        review.retirement_id
    )))
    .expect("saved package");
    assert_eq!(
        verify_retention_export(&saved)
            .expect("portable integrity")
            .artifact_sha256,
        export.artifact.artifact_sha256
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retirement_inspection_refuses_replaced_capsules_and_markers_without_repairing_them() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for fault in ["directory", "marker", "marker-link", "permissions"] {
        let f = Fixture::new();
        let output = tempfile::tempdir().expect("output");
        let (mut j, op, id, name) = exported(&f, output.path()).await;
        let expected = fs::read(output.path().join(&name)).expect("bytes");
        let read = j
            .authorize_export_inspection("alice", "run-1", &op, &id, output.path())
            .expect("read");
        let write = j
            .authorize_export_retirement(&read, output.path())
            .expect("write");
        let prepared = j
            .prepare_export_retirement(&read, &write, ExportRetirementChoice::Target, "capture")
            .await
            .expect("prepare");
        let review = prepared.preview().expect("view").clone();
        j.approve_export_retirement(&read, &write, prepared, &review.digest)
            .await
            .expect("capture");
        let capsule = output.path().join(format!(
            ".orange-agent-tmp-retired-{}",
            review.retirement_id
        ));
        let mut retained = capsule.join("item");
        match fault {
            "directory" => {
                let moved = output.path().join("external-capsule");
                fs::rename(&capsule, &moved).expect("external rename");
                fs::create_dir(&capsule).expect("replacement");
                fs::set_permissions(&capsule, fs::Permissions::from_mode(0o700))
                    .expect("permissions");
                fs::copy(moved.join("_identity"), capsule.join("_identity"))
                    .expect("same marker bytes");
                fs::copy(moved.join("item"), capsule.join("item")).expect("same item bytes");
                retained = moved.join("item");
            }
            "marker" => {
                fs::write(capsule.join("_identity"), "0".repeat(32)).expect("changed marker")
            }
            "marker-link" => {
                let marker = output.path().join("external-marker");
                fs::rename(capsule.join("_identity"), &marker).expect("move marker");
                symlink(marker, capsule.join("_identity")).expect("link");
            }
            "permissions" => fs::set_permissions(&capsule, fs::Permissions::from_mode(0o755))
                .expect("permissions"),
            _ => unreachable!(),
        }
        assert!(
            j.inspect_export_retirement(&read, &review.retirement_id)
                .await
                .is_err(),
            "{fault}"
        );
        assert_eq!(
            fs::read(&retained).expect("retained bytes"),
            expected,
            "{fault}"
        );
        assert_eq!(
            j.export_retirement_outcome("alice", "run-1", &op, &review.retirement_id)
                .expect("metadata")
                .expect("record")
                .state,
            "retained"
        );
        drop(j);
        let j = f.open();
        let fresh = j
            .authorize_export_inspection("alice", "run-1", &op, &id, output.path())
            .expect("fresh");
        assert!(
            j.inspect_export_retirement(&fresh, &review.retirement_id)
                .await
                .is_err(),
            "reopen {fault}"
        );
        assert_eq!(fs::read(&retained).expect("no repair"), expected);
        assert!(!output.path().join(&name).exists());
    }
}
