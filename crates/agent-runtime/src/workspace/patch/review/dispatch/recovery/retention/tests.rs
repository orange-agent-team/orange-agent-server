use super::*;
use crate::workspace::patch::authority::EditFileGrant;
use crate::workspace::patch::review::tests::{Fixture, approved_fixture};
use std::fs;

async fn publish(j: &mut Journal, f: &Fixture, edit: &EditFileGrant, index: usize) -> String {
    let id = format!("op-{index}");
    let inv = format!("inv-{index}");
    j.connection.execute("INSERT INTO responses(invocation,run_id,attempt,digest,body) SELECT ?,run_id,attempt,digest,body FROM responses WHERE invocation='inv'",[&inv]).expect("committed response fixture");
    let digest = f.args.digest().expect("digest");
    let operation = Operation {
        id: &id,
        owner: "alice",
        run: "run",
        invocation: &inv,
        argument_digest: &digest,
        tool_version: "apply_patch.v1",
        environment: edit.environment(),
        policy_version: "policy",
        requires_approval: true,
    };
    approved_fixture(f, j, edit, &operation).await;
    let token = j
        .begin_patch_publication(edit, &f.read, &operation)
        .await
        .expect("dispatch");
    j.publish_patch_dispatch(edit, &f.read, token)
        .expect("actual publication");
    id
}
async fn completed(f: &Fixture) -> (Journal, String) {
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let id = publish(&mut j, f, &edit, 0).await;
    (j, id)
}
async fn archive(j: &mut Journal, f: &Fixture, id: &str, key: &str) -> PatchRetentionArchive {
    let review = j
        .prepare_patch_retention("alice", id, &f.read, key)
        .await
        .expect("retention review");
    j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
        .await
        .expect("explicit metadata archival")
}
#[tokio::test]
async fn retention_in_place_transfers_budget_once_and_preserves_late_writable_inode() {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let mut original = fs::File::options()
        .write(true)
        .open(f.root.path().join("file.txt"))
        .expect("old fd");
    let (mut j, id) = completed(&f).await;
    let plan = j
        .patch_publication_plan("alice", &id)
        .expect("plan")
        .expect("present");
    let before = fs::metadata(f.root.path().join(&plan.backup_name))
        .expect("original inode")
        .ino();
    assert_eq!(
        j.patch_retention_usage("alice", &f.read)
            .expect("before")
            .resource_active_records,
        1
    );
    let review = j
        .prepare_patch_retention("alice", &id, &f.read, "retain")
        .await
        .expect("review");
    assert_eq!(
        j.prepare_patch_retention("alice", &id, &f.read, "retain")
            .await
            .expect("same review"),
        review
    );
    assert!(
        j.approve_patch_retention(
            "alice",
            &f.read,
            &review.review_id,
            &"0".repeat(64),
            "alice"
        )
        .await
        .is_err()
    );
    assert!(
        j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "bob")
            .await
            .is_err()
    );
    let archived = j
        .approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
        .await
        .expect("archive");
    assert_eq!(archived.disposition, RetentionDisposition::RetainedInPlace);
    assert_eq!(
        j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
            .await
            .expect("idempotent acknowledgement"),
        archived
    );
    let usage = j.patch_retention_usage("alice", &f.read).expect("after");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (0, 1)
    );
    assert_eq!(
        usage.owner_archive_reserved_bytes,
        FILE_RESERVATION.to_string()
    );
    assert_eq!(
        fs::metadata(f.root.path().join(&plan.backup_name))
            .expect("not moved/deleted")
            .ino(),
        before
    );
    assert_eq!(fs::read_dir(f.root.path()).expect("same files").count(), 2);
    original
        .write_all(b"late after archive\n")
        .expect("old fd still live");
    original.sync_all().expect("sync");
    assert_eq!(
        fs::read(f.root.path().join(&plan.backup_name)).expect("late bytes retained"),
        b"late after archive\n"
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("target untouched"),
        b"new\n"
    );
    let inspection = j
        .inspect_patch_archive("alice", &id, &f.read)
        .await
        .expect("explicit current read");
    assert!(inspection.view().expect("snapshot").changed_since_archival);
    assert_eq!(
        inspection
            .view()
            .expect("historical receipt preserved")
            .archive,
        archived
    );
    assert!(
        j.patch_retention_archive("bob", &id)
            .expect("owner scoped")
            .is_none()
    );
    f.read.request_revoke();
    assert!(inspection.view().is_err());
    drop(inspection);
    drop(j);
    let j = f.journal();
    assert_eq!(
        j.patch_retention_archive("alice", &id)
            .expect("durable receipt"),
        Some(archived)
    );
}

#[tokio::test]
async fn changed_retained_object_rejects_approval_but_target_only_changes_do_not() {
    let f = Fixture::new();
    let (mut j, id) = completed(&f).await;
    let plan = j
        .patch_publication_plan("alice", &id)
        .expect("plan")
        .expect("present");
    let old = j
        .prepare_patch_retention("alice", &id, &f.read, "old")
        .await
        .expect("review");
    let backup = f.root.path().join(&plan.backup_name);
    fs::rename(
        &backup,
        f.root.path().join(format!("{}.saved", plan.backup_name)),
    )
    .expect("preserve original elsewhere");
    fs::write(&backup, b"old\n").expect("same bytes, new inode");
    assert!(
        j.approve_patch_retention("alice", &f.read, &old.review_id, &old.digest, "alice")
            .await
            .is_err()
    );
    assert!(
        j.prepare_patch_retention("alice", &id, &f.read, "old")
            .await
            .is_err()
    );
    let fresh = j
        .prepare_patch_retention("alice", &id, &f.read, "fresh")
        .await
        .expect("new explicit review");
    fs::write(
        f.root.path().join("file.txt"),
        b"user continues editing target\n",
    )
    .expect("independent target edit");
    j.approve_patch_retention("alice", &f.read, &fresh.review_id, &fresh.digest, "alice")
        .await
        .expect("only retained files are transferred");
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("never changed target"),
        b"user continues editing target\n"
    );
}

#[tokio::test]
async fn unknown_publication_cannot_archive_away_its_quarantine() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = crate::workspace::patch::review::tests::op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("dispatch");
    j.connection.execute_batch("CREATE TRIGGER keep_unknown BEFORE INSERT ON editing_publication_steps WHEN NEW.step=7 BEGIN SELECT RAISE(ABORT,'owned receipt fault');END;").expect("fault");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    assert!(
        j.prepare_patch_retention("alice", "op", &f.read, "cannot-erase-unknown")
            .await
            .is_err()
    );
    let usage = j.patch_retention_usage("alice", &f.read).expect("usage");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (1, 0)
    );
    assert!(
        crate::workspaces::quarantined(&j.connection, "alice", f.read.resource_identity())
            .expect("quarantine retained")
    );
}

#[tokio::test]
async fn archival_failure_rolls_back_budget_and_never_changes_files() {
    let f = Fixture::new();
    let (mut j, id) = completed(&f).await;
    let review = j
        .prepare_patch_retention("alice", &id, &f.read, "atomic")
        .await
        .expect("review");
    j.connection.execute_batch("CREATE TRIGGER archive_fault BEFORE INSERT ON events WHEN NEW.kind='patch.retention_archived_in_place' BEGIN SELECT RAISE(ABORT,'owned archival fault');END;").expect("fault after archive row insert");
    assert!(
        j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
            .await
            .is_err()
    );
    let usage = j.patch_retention_usage("alice", &f.read).expect("rollback");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (1, 0)
    );
    assert!(
        j.patch_retention_archive("alice", &id)
            .expect("no partial receipt")
            .is_none()
    );
    j.connection
        .execute_batch("DROP TRIGGER archive_fault;")
        .expect("remove fixture fault");
    j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
        .await
        .expect("one atomic metadata retry");
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no file effects")
            .count(),
        2
    );
    for table in ["patch_retention_reviews", "patch_retention_archives"] {
        assert!(
            j.connection
                .execute(&format!("DELETE FROM {table}"), [])
                .is_err()
        );
        assert!(
            j.connection
                .execute(&format!("UPDATE {table} SET body='{{}}'"), [])
                .is_err()
        );
    }
}

#[tokio::test]
async fn revoked_reopened_foreign_and_linked_sources_fail_closed() {
    let f = Fixture::new();
    let (mut j, id) = completed(&f).await;
    assert!(
        j.prepare_patch_retention("bob", &id, &f.read, "foreign")
            .await
            .is_err()
    );
    let prior = j
        .prepare_patch_retention("alice", &id, &f.read, "reopen")
        .await
        .expect("review");
    drop(j);
    let mut j = f.journal();
    assert!(
        j.approve_patch_retention("alice", &f.read, &prior.review_id, &prior.digest, "alice")
            .await
            .is_err()
    );
    let plan = j
        .patch_publication_plan("alice", &id)
        .expect("plan")
        .expect("present");
    let backup = f.root.path().join(&plan.backup_name);
    fs::rename(
        &backup,
        f.root.path().join(format!("{}.saved", plan.backup_name)),
    )
    .expect("retain source");
    let outside = tempfile::NamedTempFile::new().expect("owned outside fixture");
    std::os::unix::fs::symlink(outside.path(), &backup).expect("adversarial link");
    assert!(
        j.prepare_patch_retention("alice", &id, &f.read, "no-links")
            .await
            .is_err()
    );
    f.read.request_revoke();
    assert!(
        j.prepare_patch_retention("alice", &id, &f.read, "revoked")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn active_admission_slot_is_reusable_only_after_confirmed_archive_handoff() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    for index in 0..64 {
        fs::write(f.root.path().join("file.txt"), b"old\n")
            .expect("explicit user reset for next owned patch fixture");
        publish(&mut j, &f, &edit, index).await;
    }
    fs::write(f.root.path().join("file.txt"), b"old\n").expect("next owned input");
    let digest = f.args.digest().expect("digest");
    j.connection.execute("INSERT INTO responses(invocation,run_id,attempt,digest,body) SELECT 'inv-64',run_id,attempt,digest,body FROM responses WHERE invocation='inv'",[]).expect("next committed response fixture");
    let operation = Operation {
        id: "op-64",
        owner: "alice",
        run: "run",
        invocation: "inv-64",
        argument_digest: &digest,
        tool_version: "apply_patch.v1",
        environment: edit.environment(),
        policy_version: "policy",
        requires_approval: true,
    };
    approved_fixture(&f, &mut j, &edit, &operation).await;
    assert!(matches!(
        j.begin_patch_publication(&edit, &f.read, &operation).await,
        Err(PatchError::Limit)
    ));
    let review = j
        .prepare_patch_retention("alice", "op-0", &f.read, "handoff")
        .await
        .expect("preview doesn't release slot");
    assert!(matches!(
        j.begin_patch_publication(&edit, &f.read, &operation).await,
        Err(PatchError::Limit)
    ));
    j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
        .await
        .expect("confirmed handoff");
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("one active slot released");
    j.publish_patch_dispatch(&edit, &f.read, token)
        .expect("new actual publication");
    let usage = j
        .patch_retention_usage("alice", &f.read)
        .expect("accounting");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (64, 1)
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("all originals retained")
            .count(),
        66
    );
}

#[tokio::test]
async fn owner_archive_budget_is_bounded_across_physical_workspaces() {
    let first = Fixture::new();
    let mut j = first.journal();
    first.response(&j, true);
    let mut retained = vec![first];
    let admitted = (ARCHIVE_LIMIT / FILE_RESERVATION) as usize;
    for index in 0..=admitted {
        if index > 0 {
            retained.push(Fixture::new());
        }
        let f = &retained[index];
        let edit = j
            .authorize_file_edit("alice", &format!("edit-{index}"), "policy", f.root.path())
            .expect("explicit fixture consent");
        let id = publish(&mut j, f, &edit, index).await;
        let review = j
            .prepare_patch_retention("alice", &id, &f.read, &format!("archive-{index}"))
            .await
            .expect("review");
        let result = j
            .approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
            .await;
        if index < admitted {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(PatchError::Limit)));
            let usage = j
                .patch_retention_usage("alice", &f.read)
                .expect("no partial release on full archive");
            assert_eq!(usage.resource_active_records, 1);
            assert_eq!(usage.owner_archive_records, admitted as i64);
            assert!(
                j.patch_retention_archive("alice", &id)
                    .expect("no receipt")
                    .is_none()
            );
        }
        j.revoke_file_edit(
            "alice",
            edit.consent_id(),
            &edit.consent_version().expect("version"),
        )
        .expect("close fixture's independent edit consent");
    }
    assert!(
        retained
            .iter()
            .all(|f| fs::read_dir(f.root.path()).expect("retained data").count() == 2)
    );
}

#[tokio::test]
async fn independently_resolved_unknown_can_archive_both_retained_versions() {
    use super::super::resolution::PatchRecoveryChoice;
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = crate::workspace::patch::review::tests::op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("dispatch");
    j.connection.execute_batch("CREATE TRIGGER capture_fault BEFORE INSERT ON editing_publication_steps WHEN NEW.step=4 BEGIN SELECT RAISE(ABORT,'owned gap fixture');END;").expect("fault");
    assert!(j.publish_patch_dispatch(&edit, &f.read, token).is_err());
    j.connection
        .execute_batch("DROP TRIGGER capture_fault;")
        .expect("remove fault");
    fs::write(f.root.path().join("file.txt"), b"chosen user version\n").expect("user target");
    let recovery = j
        .authorize_patch_recovery("alice", "policy", "op", f.root.path())
        .expect("separate recovery consent");
    let review = j
        .prepare_patch_resolution(
            &recovery,
            &f.read,
            "keep-target",
            PatchRecoveryChoice::KeepCurrentTarget,
        )
        .await
        .expect("recovery review");
    j.approve_patch_resolution(
        &recovery,
        &f.read,
        &review.recovery_id,
        &review.digest,
        "alice",
        now().expect("clock") + 60,
    )
    .await
    .expect("recovery approval");
    let token = j
        .dispatch_patch_resolution(&recovery, &f.read, &review.recovery_id)
        .await
        .expect("recovery dispatch");
    j.publish_patch_resolution(&recovery, &f.read, token)
        .expect("separate resolution");
    let archived = archive(&mut j, &f, "op", "retain-both").await;
    assert_eq!(
        archived.archive_reserved_bytes,
        (2 * FILE_RESERVATION).to_string()
    );
    assert_eq!(
        j.state("alice", "op").expect("original history").as_deref(),
        Some("unknown")
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("all three versions remain")
            .count(),
        3
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("target unchanged"),
        b"chosen user version\n"
    );
    let usage = j.patch_retention_usage("alice", &f.read).expect("usage");
    assert_eq!(
        (usage.resource_active_records, usage.owner_archive_records),
        (0, 1)
    );
}

#[test]
fn process_exit_before_or_after_archive_commit_preserves_atomic_accounting() {
    use std::os::unix::fs::PermissionsExt;
    const TEST: &str = "workspace::patch::review::dispatch::recovery::retention::tests::process_exit_before_or_after_archive_commit_preserves_atomic_accounting";
    if let Some(base) = std::env::var_os("ORANGE_RETENTION_CRASH_ROOT") {
        let base = std::path::PathBuf::from(base);
        let boundary: u8 = std::env::var("ORANGE_RETENTION_CRASH_BOUNDARY")
            .expect("boundary")
            .parse()
            .expect("integer");
        assert!((1..=2).contains(&boundary));
        let private = tempfile::tempdir_in(&base).expect("private");
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700))
            .expect("private mode");
        let f = Fixture::with_dirs(private, tempfile::tempdir_in(&base).expect("workspace"));
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let (mut j,id)=completed(&f).await;
            let review=j.prepare_patch_retention("alice",&id,&f.read,"crash").await.expect("review");
            let description=serde_json::json!({"private":f.private.path().file_name().expect("name").to_str().expect("ASCII temp name"),"root":f.root.path().file_name().expect("name").to_str().expect("ASCII temp name"),"id":id,"review":review});
            fs::write(base.join("fixture.json"),serde_json::to_vec(&description).expect("JSON")).expect("owned descriptor");
            if boundary==2 {j.approve_patch_retention("alice",&f.read,&review.review_id,&review.digest,"alice").await.expect("archive commit");}
            std::process::exit(75);
        });
    }
    for boundary in 1..=2 {
        let base = tempfile::tempdir().expect("parent owns fixtures");
        let log = fs::File::create(base.path().join("child.log")).expect("log");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env("ORANGE_RETENTION_CRASH_ROOT", base.path())
            .env("ORANGE_RETENTION_CRASH_BOUNDARY", boundary.to_string())
            .stdout(log.try_clone().expect("stdout"))
            .stderr(log)
            .spawn()
            .expect("owned child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().expect("owned status") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("stop owned child");
                child.wait().expect("reap owned child");
                panic!("retention fixture exceeded bound");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(status.code(), Some(75));
        let description: serde_json::Value = serde_json::from_slice(
            &fs::read(base.path().join("fixture.json")).expect("descriptor"),
        )
        .expect("JSON");
        let own = |key: &str| {
            let name = description[key].as_str().expect("basename");
            assert_eq!(std::path::Path::new(name).components().count(), 1);
            let p = base.path().join(name).canonicalize().expect("path");
            assert!(p.starts_with(base.path().canonicalize().expect("base")));
            p
        };
        let private = own("private");
        let root = own("root");
        let id = description["id"].as_str().expect("operation");
        let j = Journal::open(&private.join("journal.sqlite"))
            .expect("reopen after actual process exit");
        let read = WorkspaceGrant::open(
            "alice",
            "new-read-after-restart",
            "policy",
            &root,
            &[private],
        )
        .expect("explicit current read");
        let usage = j.patch_retention_usage("alice", &read).expect("usage");
        if boundary == 1 {
            assert_eq!(
                (usage.resource_active_records, usage.owner_archive_records),
                (1, 0)
            );
            assert!(
                j.patch_retention_archive("alice", id)
                    .expect("no archive")
                    .is_none()
            );
        } else {
            assert_eq!(
                (usage.resource_active_records, usage.owner_archive_records),
                (0, 1)
            );
            assert!(
                j.patch_retention_archive("alice", id)
                    .expect("committed archive")
                    .is_some()
            );
        }
        assert_eq!(
            fs::read_dir(root)
                .expect("files preserved at both boundaries")
                .count(),
            2
        );
    }
}
