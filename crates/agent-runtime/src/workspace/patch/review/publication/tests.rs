use super::super::dispatch::recovery::inspection::RecoveryFileObservation;
use super::super::tests::{Fixture, approved_fixture, op};
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

#[tokio::test]
async fn approved_patch_history_requires_version_aware_validation() {
    use crate::workspace::{WorkspaceRequest, WorkspaceResult};
    let mut f = Fixture::new();
    f.args.patch = "@@ -1 +1 @@\n-old\n+new approved version\n".into();
    let requests = [
        WorkspaceRequest::parse("read_file", &serde_json::json!({"path":"file.txt"}))
            .expect("read"),
        WorkspaceRequest::parse("list_directory", &serde_json::json!({"path":"."})).expect("list"),
        WorkspaceRequest::parse(
            "search_files",
            &serde_json::json!({"path":".","glob":"**/*"}),
        )
        .expect("names"),
        WorkspaceRequest::parse(
            "search_text",
            &serde_json::json!({"path":".","query":"old"}),
        )
        .expect("text"),
    ];
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let mut before = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        before.push(
            crate::workspace::patch::lineage::tests::source(
                &mut j,
                &f,
                "run",
                &format!("source-{index}"),
                request.clone(),
            )
            .await,
        );
    }
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("dispatch");
    j.publish_patch_dispatch(&edit, &f.read, token)
        .expect("real approved patch");
    let mut invalidated = Vec::new();
    for (request, prior) in requests.iter().zip(&before) {
        let current = f
            .read
            .execute("alice", f.read.environment(), "policy", request.clone())
            .await
            .expect("real current read");
        if let WorkspaceResult::SearchFiles(result) = current.result() {
            assert_eq!(result.paths, vec!["file.txt"]);
            assert_eq!(result.scanned_entries, 2);
            assert!(result.omitted_unsupported);
        }
        if serde_json::to_value(current.result()).expect("current result") != *prior {
            invalidated.push(request.name());
        }
    }
    assert_eq!(
        invalidated,
        vec!["read_file", "list_directory", "search_files", "search_text"]
    );
    for (index, prior) in before.iter().enumerate() {
        let validated = j
            .revalidate_workspace_history("alice", "run", &format!("source-{index}"), &f.read)
            .await
            .expect("version-aware source validation");
        assert_eq!(
            serde_json::to_value(validated.result()).expect("historical result"),
            *prior
        );
    }
}

async fn ready(f: &Fixture) -> (Journal, EditFileGrant, PatchDispatch) {
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    approved_fixture(f, &mut j, &edit, &operation).await;
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("dispatch");
    (j, edit, token)
}
fn steps(j: &Journal) -> i64 {
    j.connection
        .query_row(
            "SELECT coalesce(max(step),0) FROM editing_publication_steps",
            [],
            |r| r.get(0),
        )
        .expect("durable stages")
}

#[tokio::test]
async fn real_publication_preserves_source_inode_metadata_and_durable_receipt() {
    let f = Fixture::new();
    let path = f.root.path().join("file.txt");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("source mode");
    assert!(
        std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&path)
            .status()
            .expect("ACL")
            .success()
    );
    let original = fs::File::open(&path).expect("original fd");
    rustix::fs::fsetxattr(
        &original,
        "com.orangeagent.fixture",
        b"retained",
        rustix::fs::XattrFlags::empty(),
    )
    .expect("xattr");
    let metadata = orange_platform_files::security_metadata(&original).expect("source metadata");
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    let receipt = j
        .publish_patch_dispatch(&edit, &f.read, token)
        .expect("actual publication");
    assert_eq!(fs::read(&path).expect("new contents"), b"new\n");
    assert_eq!(
        fs::read(f.root.path().join(&receipt.retained_original)).expect("retained original"),
        b"old\n"
    );
    assert!(!f.root.path().join(plan.staged_name).exists());
    let new = fs::File::open(&path).expect("new fd");
    let backup = fs::File::open(f.root.path().join(&receipt.retained_original)).expect("backup fd");
    assert_eq!(
        rustix::fs::fstat(&original).expect("identity").st_ino,
        rustix::fs::fstat(&backup).expect("backup identity").st_ino
    );
    assert_ne!(
        rustix::fs::fstat(&new).expect("new identity").st_ino,
        rustix::fs::fstat(&original).expect("old identity").st_ino
    );
    assert_eq!(
        orange_platform_files::security_metadata(&new).expect("new security"),
        metadata
    );
    assert_eq!(
        orange_platform_files::security_metadata(&backup).expect("original security"),
        metadata
    );
    assert_eq!(steps(&j), 7);
    let view = j
        .patch_recovery_view("alice", "op")
        .expect("recovery metadata")
        .expect("owned plan");
    assert_eq!(
        view.last_durable_stage,
        Some(super::super::dispatch::recovery::PublicationStage::Confirmed)
    );
    assert_eq!(view.committed_receipt, Some(receipt.clone()));
    assert!(
        j.patch_recovery_view("bob", "op")
            .expect("other owner")
            .is_none()
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("succeeded")
    );
    assert!(
        j.connection
            .execute("DELETE FROM editing_publication_steps", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("UPDATE editing_publication_steps SET body='{}'", [])
            .is_err()
    );
    drop(j);
    let mut reopened = f.journal();
    assert_eq!(
        reopened.state("alice", "op").expect("state").as_deref(),
        Some("succeeded")
    );
    assert_eq!(steps(&reopened), 7);
    let digest = f.args.digest().expect("digest");
    assert!(
        reopened
            .begin_patch_publication(&edit, &f.read, &op(&edit, &digest))
            .await
            .is_err()
    );
    assert_eq!(fs::read(path).expect("not replayed"), b"new\n");
}

#[tokio::test]
async fn concurrent_new_target_is_never_overwritten_and_all_three_versions_survive() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    let outcome = j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
        if stage == 4 {
            fs::write(f.root.path().join("file.txt"), b"user replacement\n")
                .expect("competing writer");
        }
        Ok(())
    });
    assert!(outcome.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("competing version"),
        b"user replacement\n"
    );
    assert_eq!(
        fs::read(f.root.path().join(plan.backup_name)).expect("original"),
        b"old\n"
    );
    assert_eq!(
        fs::read(f.root.path().join(plan.staged_name)).expect("proposed"),
        b"new\n"
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("unknown")
    );
    assert_eq!(steps(&j), 5);
    let view = j
        .patch_recovery_view("alice", "op")
        .expect("uncertain metadata")
        .expect("plan");
    assert_eq!(view.execution_state, "unknown");
    assert_eq!(
        view.last_durable_stage,
        Some(super::super::dispatch::recovery::PublicationStage::PublishIntent)
    );
    assert!(view.committed_receipt.is_none());
}

#[tokio::test]
async fn old_descriptor_writes_are_preserved_and_stop_publication_when_observed() {
    let f = Fixture::new();
    let mut old = fs::OpenOptions::new()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .expect("external old fd");
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    let outcome = j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
        if stage == 4 {
            old.write_all(b"old fd change\n")
                .expect("concurrent fd write");
            old.sync_all().expect("sync");
        }
        Ok(())
    });
    assert!(outcome.is_err());
    assert!(!f.root.path().join("file.txt").exists());
    assert_eq!(
        fs::read(f.root.path().join(plan.backup_name)).expect("late original write retained"),
        b"old fd change\n"
    );
    assert_eq!(
        fs::read(f.root.path().join(plan.staged_name)).expect("new retained"),
        b"new\n"
    );
    assert_eq!(steps(&j), 4);
}

#[tokio::test]
async fn pre_capture_changes_leave_user_target_untouched() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    let outcome = j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
        if stage == 2 {
            fs::write(f.root.path().join("file.txt"), b"external edit\n").expect("external write");
        }
        Ok(())
    });
    assert!(outcome.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("kept"),
        b"external edit\n"
    );
    assert!(!f.root.path().join(plan.backup_name).exists());
    assert_eq!(
        fs::read(f.root.path().join(plan.staged_name)).expect("staged retained"),
        b"new\n"
    );
    assert_eq!(steps(&j), 2);
}

#[tokio::test]
async fn injected_stage_failures_reopen_unknown_without_automatic_filesystem_recovery() {
    for failure in [2, 4, 6, 7] {
        let f = Fixture::new();
        let (mut j, edit, token) = ready(&f).await;
        let plan = token.plan().clone();
        assert!(
            j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
                if stage == failure {
                    Err(PatchError::RecoveryRequired)
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        let expected_step = if failure == 7 { 6 } else { failure as i64 };
        assert_eq!(steps(&j), expected_step);
        let target = f.root.path().join("file.txt");
        if failure == 2 {
            assert_eq!(fs::read(&target).expect("old target"), b"old\n");
        } else if failure == 4 {
            assert!(!target.exists());
        } else {
            assert_eq!(fs::read(&target).expect("new target"), b"new\n");
        }
        if failure >= 4 {
            assert_eq!(
                fs::read(f.root.path().join(&plan.backup_name)).expect("original retained"),
                b"old\n"
            );
        }
        drop(j);
        let reopened = f.journal();
        assert_eq!(
            reopened.state("alice", "op").expect("unknown").as_deref(),
            Some("unknown")
        );
        assert_eq!(steps(&reopened), expected_step);
        assert_eq!(
            reopened
                .patch_publication_plan("alice", "op")
                .expect("plan"),
            Some(plan)
        );
        if failure == 4 {
            assert!(!target.exists());
        }
    }
}

#[tokio::test]
async fn stage_name_collision_and_revocation_never_overwrite_source_or_collision() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let staged = f.root.path().join(&token.plan().staged_name);
    fs::write(&staged, b"collision").expect("owned collision");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    assert_eq!(fs::read(staged).expect("collision kept"), b"collision");
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("source kept"),
        b"old\n"
    );
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    edit.request_revoke();
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    assert_eq!(steps(&j), 0);
    assert_eq!(
        j.patch_recovery_view("alice", "op")
            .expect("no effect evidence")
            .expect("plan")
            .last_durable_stage,
        None
    );
    assert_eq!(fs::read_dir(f.root.path()).expect("no effects").count(), 1);
}

#[tokio::test]
async fn failure_to_save_final_receipt_retains_both_versions_and_never_marks_success() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    j.connection.execute_batch("CREATE TRIGGER fail_confirmation BEFORE INSERT ON editing_publication_steps WHEN NEW.step=7 BEGIN SELECT RAISE(ABORT,'injected receipt failure');END;").expect("fault");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("published but unconfirmed"),
        b"new\n"
    );
    assert_eq!(
        fs::read(f.root.path().join(&plan.backup_name)).expect("original retained"),
        b"old\n"
    );
    assert_eq!(steps(&j), 6);
    let receipt: Option<String> = j
        .connection
        .query_row(
            "SELECT result_json FROM operations WHERE operation_id='op'",
            [],
            |r| r.get(0),
        )
        .expect("receipt");
    assert!(receipt.is_none());
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("unknown")
    );
    drop(j);
    let reopened = f.journal();
    assert_eq!(steps(&reopened), 6);
    assert_eq!(
        reopened.state("alice", "op").expect("state").as_deref(),
        Some("unknown")
    );
}

#[tokio::test]
async fn expired_approval_after_dispatch_cannot_create_even_a_staging_file() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    j.connection
        .execute(
            "UPDATE approvals SET expires_at=0 WHERE operation_id='op'",
            [],
        )
        .expect("expiry fixture");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    assert_eq!(steps(&j), 0);
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no staging files")
            .count(),
        1
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
}

#[tokio::test]
async fn original_inode_is_not_deleted_after_success_so_late_fd_writes_survive() {
    let f = Fixture::new();
    let mut old = fs::OpenOptions::new()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .expect("old writable fd");
    let (mut j, edit, token) = ready(&f).await;
    let receipt = j
        .publish_patch_dispatch(&edit, &f.read, token)
        .expect("publish");
    old.write_all(b"write after receipt\n")
        .expect("late external write");
    old.sync_all().expect("sync");
    assert_eq!(
        fs::read(f.root.path().join(receipt.retained_original)).expect("late bytes survive"),
        b"write after receipt\n"
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("new published version"),
        b"new\n"
    );
}

#[test]
fn abrupt_process_exit_preserves_recovery_evidence_across_every_filesystem_boundary() {
    const TEST: &str = "workspace::patch::review::publication::tests::abrupt_process_exit_preserves_recovery_evidence_across_every_filesystem_boundary";
    if let Some(base) = std::env::var_os("ORANGE_PATCH_CRASH_ROOT") {
        let base = std::path::PathBuf::from(base);
        let failure: u8 = std::env::var("ORANGE_PATCH_CRASH_STAGE")
            .expect("stage")
            .parse()
            .expect("integer stage");
        assert!((2..=7).contains(&failure));
        let private = tempfile::tempdir_in(&base).expect("private fixture");
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700))
            .expect("private mode");
        let f = Fixture::with_dirs(
            private,
            tempfile::tempdir_in(&base).expect("workspace fixture"),
        );
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let (mut j,edit,token)=ready(&f).await;
            let description=serde_json::json!({"private":f.private.path().file_name().expect("private name").to_str().expect("ASCII fixture name"),"root":f.root.path().file_name().expect("root name").to_str().expect("ASCII fixture name"),"plan":token.plan()});
            fs::write(base.join("fixture.json"),serde_json::to_vec(&description).expect("descriptor")).expect("persist owned fixture descriptor");
            let _=j.publish_patch_with_hook(&edit,&f.read,token,|stage|{
                if stage==failure { std::process::exit(73); }
                Ok(())
            });
        });
        panic!("expected abrupt exit at the selected boundary");
    }
    for failure in 2..=7 {
        let base = tempfile::tempdir().expect("parent owns all child fixtures");
        let log = fs::File::create(base.path().join("child.log")).expect("log");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env("ORANGE_PATCH_CRASH_ROOT", base.path())
            .env("ORANGE_PATCH_CRASH_STAGE", failure.to_string())
            .stdout(log.try_clone().expect("stdout"))
            .stderr(log)
            .spawn()
            .expect("owned child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().expect("owned child status") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("stop owned timed-out child");
                child.wait().expect("reap owned child");
                panic!("publication child exceeded its bounded wait");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(73),
            "child failed before selected boundary"
        );
        let descriptor: serde_json::Value = serde_json::from_slice(
            &fs::read(base.path().join("fixture.json")).expect("descriptor"),
        )
        .expect("JSON");
        let own_path = |key: &str| {
            let name = descriptor[key].as_str().expect("child basename");
            assert_eq!(std::path::Path::new(name).components().count(), 1);
            let path = base.path().join(name).canonicalize().expect("owned path");
            assert!(path.starts_with(base.path().canonicalize().expect("base")));
            path
        };
        let root = own_path("root");
        let plan: super::super::dispatch::PublicationPlan =
            serde_json::from_value(descriptor["plan"].clone()).expect("plan");
        let journal = Journal::open(&own_path("private").join("journal.sqlite"))
            .expect("real reopen after process death");
        assert_eq!(
            journal.state("alice", "op").expect("unknown").as_deref(),
            Some("unknown")
        );
        let expected_step = match failure {
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 5,
            _ => 6,
        };
        assert_eq!(steps(&journal), expected_step);
        assert_eq!(
            journal
                .patch_publication_plan("alice", "op")
                .expect("plan survives"),
            Some(plan.clone())
        );
        let target = root.join("file.txt");
        match failure {
            2 => assert_eq!(fs::read(target).expect("old target"), b"old\n"),
            3 | 4 => assert!(!target.exists()),
            _ => assert_eq!(fs::read(target).expect("new target"), b"new\n"),
        }
        if failure >= 3 {
            assert_eq!(
                fs::read(root.join(plan.backup_name)).expect("original retained"),
                b"old\n"
            );
        }
        if failure < 5 {
            assert_eq!(
                fs::read(root.join(plan.staged_name)).expect("staged retained"),
                b"new\n"
            );
        }
    }
}

#[tokio::test]
async fn explicit_read_capability_inspects_unknown_versions_without_content_or_mutation() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    assert!(
        j.publish_patch_with_hook(&edit, &f.read, token, |stage| if stage == 4 {
            Err(PatchError::RecoveryRequired)
        } else {
            Ok(())
        })
        .is_err()
    );
    let receipt = j
        .inspect_patch_recovery("alice", "op", &f.read)
        .await
        .expect("read-only inspection");
    let view = receipt.snapshot().expect("live view");
    assert_eq!(view.journal.execution_state, "unknown");
    assert!(matches!(view.target, RecoveryFileObservation::Missing));
    assert!(matches!(
        view.staged,
        RecoveryFileObservation::Regular {
            matches_result_content: true,
            reviewed_original_inode: false,
            ..
        }
    ));
    assert!(matches!(
        view.original,
        RecoveryFileObservation::Regular {
            matches_original_content: true,
            reviewed_original_inode: true,
            ..
        }
    ));
    let encoded = serde_json::to_string(view).expect("metadata JSON");
    assert!(!encoded.contains("old\\n") && !encoded.contains("new\\n"));
    assert!(!f.root.path().join("file.txt").exists());
    assert_eq!(
        fs::read(f.root.path().join(&plan.staged_name)).expect("staging unchanged"),
        b"new\n"
    );
    assert_eq!(
        fs::read(f.root.path().join(&plan.backup_name)).expect("backup unchanged"),
        b"old\n"
    );
    f.read.request_revoke();
    assert!(receipt.snapshot().is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), f.read.wait_drained())
            .await
            .is_err()
    );
    drop(receipt);
    tokio::time::timeout(std::time::Duration::from_secs(1), f.read.wait_drained())
        .await
        .expect("drained");
    assert!(
        j.inspect_patch_recovery("alice", "op", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn recovery_inspection_rejects_foreign_owner_directory_and_links() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    let plan = token.plan().clone();
    assert!(
        j.publish_patch_with_hook(&edit, &f.read, token, |stage| if stage == 4 {
            Err(PatchError::RecoveryRequired)
        } else {
            Ok(())
        })
        .is_err()
    );
    assert!(
        j.inspect_patch_recovery("bob", "op", &f.read)
            .await
            .is_err()
    );
    let foreign = tempfile::tempdir().expect("foreign owned fixture");
    let wrong = WorkspaceGrant::open(
        "alice",
        "different-root",
        "policy",
        foreign.path(),
        &[f.private.path().into()],
    )
    .expect("foreign read grant");
    assert!(
        j.inspect_patch_recovery("alice", "op", &wrong)
            .await
            .is_err()
    );
    let secret = foreign.path().join("private.txt");
    fs::write(&secret, b"owned confidential outside fixture").expect("outside fixture");
    let backup = f.root.path().join(&plan.backup_name);
    fs::rename(
        &backup,
        f.root.path().join(format!("{}.saved", plan.backup_name)),
    )
    .expect("retain actual original");
    std::os::unix::fs::symlink(&secret, &backup).expect("adversarial link");
    let inspected = j
        .inspect_patch_recovery("alice", "op", &f.read)
        .await
        .expect("reports refused link");
    assert!(matches!(
        inspected.snapshot().expect("snapshot").original,
        RecoveryFileObservation::Refused { .. }
    ));
    assert!(
        !serde_json::to_string(inspected.snapshot().expect("snapshot"))
            .expect("JSON")
            .contains("confidential")
    );
    assert_eq!(
        fs::read(secret).expect("external untouched"),
        b"owned confidential outside fixture"
    );
}

#[tokio::test]
async fn recovery_inspection_detects_late_old_inode_updates_without_claiming_receipt_changed() {
    let f = Fixture::new();
    let mut old = fs::File::options()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .expect("old fd");
    let (mut j, edit, token) = ready(&f).await;
    let published = j
        .publish_patch_dispatch(&edit, &f.read, token)
        .expect("publish");
    old.write_all(b"late original version\n")
        .expect("late update");
    old.sync_all().expect("sync");
    let receipt = j
        .inspect_patch_recovery("alice", "op", &f.read)
        .await
        .expect("inspect current versions");
    let view = receipt.snapshot().expect("snapshot");
    assert_eq!(view.journal.committed_receipt.as_ref(), Some(&published));
    assert!(matches!(
        view.target,
        RecoveryFileObservation::Regular {
            matches_result_content: true,
            ..
        }
    ));
    assert!(matches!(
        view.original,
        RecoveryFileObservation::Regular {
            reviewed_original_inode: true,
            matches_original_content: false,
            ..
        }
    ));
    assert!(matches!(view.staged, RecoveryFileObservation::Missing));
}

#[tokio::test]
async fn corrupted_success_receipt_is_not_presented_as_committed_recovery_evidence() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    j.publish_patch_dispatch(&edit, &f.read, token)
        .expect("publish");
    j.connection
        .execute(
            "UPDATE operations SET result_digest=? WHERE operation_id='op'",
            ["0".repeat(64)],
        )
        .expect("owned corrupt-database fixture");
    assert!(j.patch_recovery_view("alice", "op").is_err());
    assert!(
        j.inspect_patch_recovery("alice", "op", &f.read)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no filesystem effects"),
        b"new\n"
    );
}
