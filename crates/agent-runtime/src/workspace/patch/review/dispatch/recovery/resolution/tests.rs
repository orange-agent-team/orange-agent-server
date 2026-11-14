use super::*;
use crate::workspace::patch::review::tests::{Fixture, approved_fixture, op};
use std::fs;

async fn interrupted(f: &Fixture) -> Journal {
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
    j.connection.execute_batch("CREATE TRIGGER capture_fault BEFORE INSERT ON editing_publication_steps WHEN NEW.step=4 BEGIN SELECT RAISE(ABORT,'owned capture failure fixture');END;").expect("fault");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    j.connection
        .execute_batch("DROP TRIGGER capture_fault;")
        .expect("remove fixture fault");
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("unknown")
    );
    assert!(!f.root.path().join(&f.args.path).exists());
    j
}
fn recovery_grant(j: &Journal, f: &Fixture) -> PatchRecoveryGrant {
    j.authorize_patch_recovery("alice", "policy", "op", f.root.path())
        .expect("separate explicit recovery consent")
}
async fn approved(
    j: &mut Journal,
    f: &Fixture,
    grant: &PatchRecoveryGrant,
    key: &str,
    choice: PatchRecoveryChoice,
) -> PatchResolutionReview {
    let review = j
        .prepare_patch_resolution(grant, &f.read, key, choice)
        .await
        .expect("review");
    j.approve_patch_resolution(
        grant,
        &f.read,
        &review.recovery_id,
        &review.digest,
        "alice",
        now().expect("clock") + 300,
    )
    .await
    .expect("exact owner approval");
    review
}
fn action_state(j: &Journal, id: &str) -> String {
    j.connection
        .query_row(
            "SELECT state FROM patch_recoveries WHERE recovery_id=?",
            [id],
            |r| r.get(0),
        )
        .expect("recovery state")
}

#[tokio::test]
async fn explicit_restoration_or_staged_publication_preserves_other_versions_and_unknown_history() {
    for choice in [
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
        PatchRecoveryChoice::PublishStagedToMissingTarget,
    ] {
        let f = Fixture::new();
        use std::os::unix::fs::PermissionsExt;
        let source = f.root.path().join("file.txt");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("original mode");
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read"])
                .arg(&source)
                .status()
                .expect("original ACL")
                .success()
        );
        let original_fd = fs::File::open(&source).expect("source fd");
        rustix::fs::fsetxattr(
            &original_fd,
            "com.orangeagent.recoveryfixture",
            b"preserve",
            rustix::fs::XattrFlags::empty(),
        )
        .expect("original xattr");
        let security =
            orange_platform_files::security_metadata(&original_fd).expect("original security");
        let mut j = interrupted(&f).await;
        let plan = j
            .patch_publication_plan("alice", "op")
            .expect("plan")
            .expect("present");
        assert!(
            j.authorize_file_edit("alice", "before-resolution", "policy", f.root.path())
                .is_err()
        );
        let grant = recovery_grant(&j, &f);
        let review = j
            .prepare_patch_resolution(&grant, &f.read, "decision", choice)
            .await
            .expect("review");
        assert_eq!(
            j.prepare_patch_resolution(&grant, &f.read, "decision", choice)
                .await
                .expect("idempotent review"),
            review
        );
        assert!(
            j.dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
                .await
                .is_err()
        );
        assert!(
            j.approve_patch_resolution(
                &grant,
                &f.read,
                &review.recovery_id,
                &"0".repeat(64),
                "alice",
                now().expect("clock") + 60
            )
            .await
            .is_err()
        );
        assert!(
            j.approve_patch_resolution(
                &grant,
                &f.read,
                &review.recovery_id,
                &review.digest,
                "bob",
                now().expect("clock") + 60
            )
            .await
            .is_err()
        );
        j.approve_patch_resolution(
            &grant,
            &f.read,
            &review.recovery_id,
            &review.digest,
            "alice",
            now().expect("clock") + 60,
        )
        .await
        .expect("approve");
        let dispatch = j
            .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
            .await
            .expect("durable recovery barrier");
        assert!(dispatch.has_live_directory_access());
        assert!(
            j.dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
                .await
                .is_err()
        );
        let result = j
            .publish_patch_resolution(&grant, &f.read, dispatch)
            .expect("actual resolution");
        let expected = if choice == PatchRecoveryChoice::RestoreOriginalToMissingTarget {
            b"old\n"
        } else {
            b"new\n"
        };
        assert_eq!(
            fs::read(f.root.path().join("file.txt")).expect("restored path"),
            expected
        );
        assert_eq!(
            orange_platform_files::security_metadata(
                &fs::File::open(f.root.path().join("file.txt")).expect("restored fd")
            )
            .expect("restored security"),
            security
        );
        let kept = if choice == PatchRecoveryChoice::RestoreOriginalToMissingTarget {
            plan.staged_name
        } else {
            plan.backup_name
        };
        assert!(f.root.path().join(kept).is_file());
        assert_eq!(
            fs::read_dir(f.root.path())
                .expect("all retained names")
                .count(),
            2
        );
        assert_eq!(
            j.state("alice", "op").expect("original history").as_deref(),
            Some("unknown")
        );
        assert_eq!(action_state(&j, &review.recovery_id), "succeeded");
        assert_eq!(
            j.patch_resolution_result("alice", "op").expect("receipt"),
            Some(result.clone())
        );
        assert!(
            j.patch_resolution_result("bob", "op")
                .expect("owner isolation")
                .is_none()
        );
        assert!(
            !crate::workspaces::quarantined(&j.connection, "alice", f.read.resource_identity())
                .expect("only solved effect released")
        );
        j.authorize_file_edit("alice", "after-resolution", "policy", f.root.path())
            .expect("fresh independent edit consent now possible");
        assert!(
            j.prepare_patch_resolution(&grant, &f.read, "reuse", choice)
                .await
                .is_err()
        );
        drop(j);
        let j = f.journal();
        assert_eq!(
            j.patch_resolution_result("alice", "op")
                .expect("durable receipt"),
            Some(result)
        );
        assert_eq!(
            j.state("alice", "op")
                .expect("original still unknown")
                .as_deref(),
            Some("unknown")
        );
    }
}

#[tokio::test]
async fn keep_current_target_requires_its_actual_identity_and_never_overwrites_it() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let mut j = interrupted(&f).await;
    let target = f.root.path().join("file.txt");
    fs::write(&target, b"user version\n").expect("competing target");
    let grant = recovery_grant(&j, &f);
    assert!(
        j.prepare_patch_resolution(
            &grant,
            &f.read,
            "cannot-overwrite",
            PatchRecoveryChoice::RestoreOriginalToMissingTarget
        )
        .await
        .is_err()
    );
    let old = j
        .prepare_patch_resolution(
            &grant,
            &f.read,
            "keep-old",
            PatchRecoveryChoice::KeepCurrentTarget,
        )
        .await
        .expect("review");
    fs::rename(&target, f.root.path().join("user-version-saved")).expect("retain external version");
    fs::write(&target, b"user version\n").expect("same bytes different inode");
    assert!(
        j.approve_patch_resolution(
            &grant,
            &f.read,
            &old.recovery_id,
            &old.digest,
            "alice",
            now().expect("clock") + 60
        )
        .await
        .is_err()
    );
    assert!(
        j.prepare_patch_resolution(
            &grant,
            &f.read,
            "keep-old",
            PatchRecoveryChoice::KeepCurrentTarget
        )
        .await
        .is_err()
    );
    let fresh = approved(
        &mut j,
        &f,
        &grant,
        "keep-new",
        PatchRecoveryChoice::KeepCurrentTarget,
    )
    .await;
    let before = fs::metadata(&target).expect("identity").ino();
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &fresh.recovery_id)
        .await
        .expect("dispatch");
    j.publish_patch_resolution(&grant, &f.read, token)
        .expect("keep current");
    assert_eq!(
        fs::metadata(&target).expect("identity preserved").ino(),
        before
    );
    assert_eq!(
        fs::read(target).expect("bytes preserved"),
        b"user version\n"
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("nothing removed")
            .count(),
        4
    );
}

#[tokio::test]
async fn revoked_expired_foreign_or_reopened_grants_cannot_restore_files() {
    let f = Fixture::new();
    let mut j = interrupted(&f).await;
    assert!(
        j.authorize_patch_recovery("bob", "policy", "op", f.root.path())
            .is_err()
    );
    assert!(
        j.authorize_patch_recovery("alice", "other-policy", "op", f.root.path())
            .is_err()
    );
    let foreign = tempfile::tempdir().expect("foreign owned root");
    assert!(
        j.authorize_patch_recovery("alice", "policy", "op", foreign.path())
            .is_err()
    );
    let grant = recovery_grant(&j, &f);
    let review = approved(
        &mut j,
        &f,
        &grant,
        "revoke",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
        .await
        .expect("dispatch");
    grant.request_revoke();
    assert!(j.publish_patch_resolution(&grant, &f.read, token).is_err());
    assert_eq!(action_state(&j, &review.recovery_id), "unknown");
    assert!(!f.root.path().join("file.txt").exists());
    let grant = recovery_grant(&j, &f);
    let review = j
        .prepare_patch_resolution(
            &grant,
            &f.read,
            "expiry",
            PatchRecoveryChoice::RestoreOriginalToMissingTarget,
        )
        .await
        .expect("review");
    j.approve_patch_resolution(
        &grant,
        &f.read,
        &review.recovery_id,
        &review.digest,
        "alice",
        now().expect("clock") + 1,
    )
    .await
    .expect("short approval");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(
        j.dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
            .await
            .is_err()
    );
    let grant = recovery_grant(&j, &f);
    let review = approved(
        &mut j,
        &f,
        &grant,
        "reopen",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    drop(j);
    let mut j = f.journal();
    assert!(
        j.dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
            .await
            .is_err()
    );
    assert!(
        j.prepare_patch_resolution(
            &grant,
            &f.read,
            "stale-authority",
            PatchRecoveryChoice::RestoreOriginalToMissingTarget
        )
        .await
        .is_err()
    );
    let new_grant = recovery_grant(&j, &f);
    assert!(
        j.dispatch_patch_resolution(&new_grant, &f.read, &review.recovery_id)
            .await
            .is_err()
    );
    assert!(!f.root.path().join("file.txt").exists());
}

#[tokio::test]
async fn competing_target_during_recovery_is_not_replaced_and_failed_action_is_not_replayed() {
    let f = Fixture::new();
    let mut j = interrupted(&f).await;
    let grant = recovery_grant(&j, &f);
    let review = approved(
        &mut j,
        &f,
        &grant,
        "race",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
        .await
        .expect("dispatch");
    assert!(
        j.resolve_patch_with_hook(&grant, &f.read, token, |stage| {
            if stage == 1 {
                fs::write(f.root.path().join("file.txt"), b"competing user write\n").expect("race");
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(action_state(&j, &review.recovery_id), "unknown");
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("not overwritten"),
        b"competing user write\n"
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("all versions kept")
            .count(),
        3
    );
    assert!(
        j.patch_resolution_result("alice", "op")
            .expect("no resolution")
            .is_none()
    );
    assert!(
        j.dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn receipt_failure_keeps_real_restoration_unknown_until_a_new_keep_decision() {
    let f = Fixture::new();
    let mut j = interrupted(&f).await;
    let grant = recovery_grant(&j, &f);
    let review = approved(
        &mut j,
        &f,
        &grant,
        "receipt-failure",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
        .await
        .expect("dispatch");
    j.connection.execute_batch("CREATE TRIGGER result_fault BEFORE INSERT ON patch_recovery_results BEGIN SELECT RAISE(ABORT,'owned receipt fault');END;").expect("fault");
    assert!(j.publish_patch_resolution(&grant, &f.read, token).is_err());
    assert_eq!(action_state(&j, &review.recovery_id), "unknown");
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("effect occurred"),
        b"old\n"
    );
    assert!(
        crate::workspaces::quarantined(&j.connection, "alice", f.read.resource_identity())
            .expect("not prematurely released")
    );
    assert!(
        j.patch_resolution_result("alice", "op")
            .expect("rolled back receipt")
            .is_none()
    );
    j.connection
        .execute_batch("DROP TRIGGER result_fault;")
        .expect("remove owned fault");
    drop(j);
    let mut j = f.journal();
    let grant = recovery_grant(&j, &f);
    let followup = approved(
        &mut j,
        &f,
        &grant,
        "new-keep-decision",
        PatchRecoveryChoice::KeepCurrentTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &followup.recovery_id)
        .await
        .expect("new explicit dispatch");
    j.publish_patch_resolution(&grant, &f.read, token)
        .expect("acknowledge actual current version");
    assert_eq!(action_state(&j, &review.recovery_id), "unknown");
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no deletion or repeat")
            .count(),
        2
    );
}

#[tokio::test]
async fn only_one_live_recovery_and_resolution_does_not_clear_another_unknown_effect() {
    let f = Fixture::new();
    let mut j = interrupted(&f).await;
    let grant = recovery_grant(&j, &f);
    let one = approved(
        &mut j,
        &f,
        &grant,
        "one",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let two = approved(
        &mut j,
        &f,
        &grant,
        "two",
        PatchRecoveryChoice::PublishStagedToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &one.recovery_id)
        .await
        .expect("first dispatch");
    assert!(
        j.dispatch_patch_resolution(&grant, &f.read, &two.recovery_id)
            .await
            .is_err()
    );
    assert!(
        j.prepare_patch_resolution(
            &grant,
            &f.read,
            "three",
            PatchRecoveryChoice::RestoreOriginalToMissingTarget
        )
        .await
        .is_err()
    );
    j.publish_patch_resolution(&grant, &f.read, token)
        .expect("resolve one effect");
    j.connection.execute("INSERT INTO operations(operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state) SELECT 'unrelated','alice',run_id,invocation,argument_digest,'external_effect.v1',environment,policy,0,'unknown' FROM operations WHERE operation_id='op'",[]).expect("explicit unrelated uncertain-effect fixture");
    assert!(
        crate::workspaces::quarantined(&j.connection, "alice", f.read.resource_identity())
            .expect("other unknown remains")
    );
    assert!(
        j.authorize_file_edit("alice", "must-stay-blocked", "policy", f.root.path())
            .is_err()
    );
    for table in [
        "patch_recoveries",
        "patch_recovery_approvals",
        "patch_recovery_results",
        "patch_recovery_resolutions",
    ] {
        assert!(
            j.connection
                .execute(&format!("DELETE FROM {table}"), [])
                .is_err()
        );
    }
    assert!(
        j.connection
            .execute("UPDATE patch_recoveries SET body='{}'", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "UPDATE patch_recovery_approvals SET expires_at=9999999999",
                []
            )
            .is_err()
    );
}

#[tokio::test]
async fn late_original_writes_change_review_and_restoring_inode_preserves_future_writes() {
    use std::io::Write;
    let f = Fixture::new();
    let mut original = fs::File::options()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .expect("old fd");
    let mut j = interrupted(&f).await;
    let grant = recovery_grant(&j, &f);
    let stale = j
        .prepare_patch_resolution(
            &grant,
            &f.read,
            "before-late-write",
            PatchRecoveryChoice::RestoreOriginalToMissingTarget,
        )
        .await
        .expect("review");
    original.write_all(b"late write\n").expect("late fd write");
    original.sync_all().expect("sync");
    assert!(
        j.approve_patch_resolution(
            &grant,
            &f.read,
            &stale.recovery_id,
            &stale.digest,
            "alice",
            now().expect("clock") + 60
        )
        .await
        .is_err()
    );
    let current = approved(
        &mut j,
        &f,
        &grant,
        "review-current-original",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &current.recovery_id)
        .await
        .expect("dispatch");
    j.publish_patch_resolution(&grant, &f.read, token)
        .expect("restore latest explicitly approved old inode");
    original
        .write_all(b"further write\n")
        .expect("same old fd still attached");
    original.sync_all().expect("sync");
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("all writes survive"),
        b"late write\nfurther write\n"
    );
}

#[test]
fn process_exit_during_recovery_never_replays_or_clears_quarantine() {
    use std::os::unix::fs::PermissionsExt;
    const TEST: &str = "workspace::patch::review::dispatch::recovery::resolution::tests::process_exit_during_recovery_never_replays_or_clears_quarantine";
    if let Some(base) = std::env::var_os("ORANGE_RESOLUTION_CRASH_ROOT") {
        let base = std::path::PathBuf::from(base);
        let failure: u8 = std::env::var("ORANGE_RESOLUTION_CRASH_STAGE")
            .expect("stage")
            .parse()
            .expect("stage integer");
        assert!((1..=3).contains(&failure));
        let private = tempfile::tempdir_in(&base).expect("private");
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700))
            .expect("private mode");
        let f = Fixture::with_dirs(private, tempfile::tempdir_in(&base).expect("workspace"));
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let mut j=interrupted(&f).await;let grant=recovery_grant(&j,&f);
            let review=approved(&mut j,&f,&grant,"crash",PatchRecoveryChoice::RestoreOriginalToMissingTarget).await;
            let token=j.dispatch_patch_resolution(&grant,&f.read,&review.recovery_id).await.expect("dispatch");
            let descriptor=serde_json::json!({"private":f.private.path().file_name().expect("name").to_str().expect("ASCII temp name"),"root":f.root.path().file_name().expect("name").to_str().expect("ASCII temp name"),"recovery_id":review.recovery_id,"resource":f.read.resource_identity()});
            fs::write(base.join("fixture.json"),serde_json::to_vec(&descriptor).expect("JSON")).expect("owned descriptor");
            let _=j.resolve_patch_with_hook(&grant,&f.read,token,|stage|{
                if stage==failure {std::process::exit(74);}Ok(())
            });
        });
        panic!("child did not exit at selected boundary");
    }
    for failure in 1..=3 {
        let base = tempfile::tempdir().expect("parent owns all fixture data");
        let log = fs::File::create(base.path().join("child.log")).expect("log");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env("ORANGE_RESOLUTION_CRASH_ROOT", base.path())
            .env("ORANGE_RESOLUTION_CRASH_STAGE", failure.to_string())
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
                child.kill().expect("stop owned child");
                child.wait().expect("reap owned child");
                panic!("recovery crash fixture timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(74),
            "child failed before the selected boundary"
        );
        let descriptor: serde_json::Value = serde_json::from_slice(
            &fs::read(base.path().join("fixture.json")).expect("descriptor"),
        )
        .expect("JSON");
        let path = |key: &str| {
            let name = descriptor[key].as_str().expect("basename");
            assert_eq!(std::path::Path::new(name).components().count(), 1);
            let result = base.path().join(name).canonicalize().expect("owned path");
            assert!(result.starts_with(base.path().canonicalize().expect("base")));
            result
        };
        let j = Journal::open(&path("private").join("journal.sqlite"))
            .expect("reopen after real process exit");
        assert_eq!(
            action_state(&j, descriptor["recovery_id"].as_str().expect("id")),
            "unknown"
        );
        assert!(
            j.patch_resolution_result("alice", "op")
                .expect("no resolution")
                .is_none()
        );
        assert!(
            crate::workspaces::quarantined(
                &j.connection,
                "alice",
                descriptor["resource"].as_str().expect("resource")
            )
            .expect("still quarantined")
        );
        let root = path("root");
        if failure == 1 {
            assert!(!root.join("file.txt").exists());
        } else {
            assert_eq!(
                fs::read(root.join("file.txt")).expect("restoration happened"),
                b"old\n"
            );
        }
        assert_eq!(fs::read_dir(root).expect("no deletion/replay").count(), 2);
    }
}

#[tokio::test]
async fn recovery_restores_native_spelling_from_the_durable_plan() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    f.args.path = "FILE.TXT".into();
    if !f.root.path().join("FILE.TXT").exists() {
        fs::write(f.root.path().join("FILE.TXT"), b"old\n")
            .expect("independent case-sensitive fixture");
    }
    let target_inode = fs::metadata(f.root.path().join("FILE.TXT"))
        .expect("target")
        .ino();
    let native = fs::read_dir(f.root.path())
        .expect("entries")
        .map(|e| e.expect("entry").file_name())
        .find(|n| fs::metadata(f.root.path().join(n)).is_ok_and(|m| m.ino() == target_inode))
        .expect("native name");
    let mut j = interrupted(&f).await;
    let plan = j
        .patch_publication_plan("alice", "op")
        .expect("plan")
        .expect("present");
    assert_eq!(plan.target_entry_name.as_deref(), native.to_str());
    let grant = recovery_grant(&j, &f);
    let review = approved(
        &mut j,
        &f,
        &grant,
        "restore-alias",
        PatchRecoveryChoice::RestoreOriginalToMissingTarget,
    )
    .await;
    let token = j
        .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
        .await
        .expect("dispatch");
    j.publish_patch_resolution(&grant, &f.read, token)
        .expect("restore");
    assert!(
        fs::read_dir(f.root.path())
            .expect("entries")
            .any(|e| e.expect("entry").file_name() == native)
    );
    assert_eq!(
        fs::read(f.root.path().join(native)).expect("restored bytes"),
        b"old\n"
    );
}
