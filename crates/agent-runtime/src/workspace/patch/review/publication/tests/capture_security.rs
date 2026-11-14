use super::*;
use crate::workspace::patch::review::dispatch::recovery::resolution::PatchRecoveryChoice;

// Controlled OS-label substitution on owned files. Real automatic labels are
// separately exercised by check-file-editing.py --sandboxed.
fn label(path: &std::path::Path, value: &[u8]) {
    let file = fs::File::open(path).expect("owned file");
    rustix::fs::fsetxattr(
        &file,
        "com.apple.quarantine",
        value,
        rustix::fs::XattrFlags::empty(),
    )
    .expect("owned label fixture");
}
#[tokio::test]
async fn unknown_stage_one_cannot_be_backfilled_with_new_capture_authority() {
    let f = Fixture::new();
    let (mut j, edit, token) = ready(&f).await;
    assert!(
        j.publish_patch_with_hook(&edit, &f.read, token, |stage| if stage == 1 {
            Err(PatchError::Invalid)
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("unknown")
    );
    assert_eq!(steps(&j), 1);
    assert!(j.connection.execute("INSERT INTO patch_capture_witnesses(operation_id,body,digest) VALUES('op','{}','untrusted')",[]).is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("source not captured"),
        b"old\n"
    );
}
#[tokio::test]
async fn capture_witness_survives_interruption_and_binds_history_to_actual_security() {
    for stop in [false, true] {
        let f = Fixture::new();
        let (mut j, edit, token) = ready(&f).await;
        let plan = token.plan().clone();
        let prior = crate::workspace::patch::lineage::tests::source(
            &mut j,
            &f,
            "run",
            "source-before-capture",
            crate::workspace::WorkspaceRequest::parse(
                "read_file",
                &serde_json::json!({"path":"file.txt"}),
            )
            .expect("read"),
        )
        .await;
        let result = j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
            if stage == 1 {
                label(
                    &f.root.path().join(&plan.staged_name),
                    b"0086;65000000;NativeFixture;",
                );
            }
            if stage == 3 {
                label(
                    &f.root.path().join(&plan.backup_name),
                    b"0086;65000001;NativeFixture;",
                );
                if stop {
                    return Err(PatchError::Invalid);
                }
            }
            Ok(())
        });
        let evidence = j.capture_evidence("alice", "op").expect("durable witness");
        assert!(evidence.witness.is_some());
        assert_eq!(evidence.retained_security_digest.is_none(), stop);
        assert!(
            j.connection
                .execute(
                    "UPDATE patch_capture_witnesses SET digest='wrong' WHERE operation_id='op'",
                    []
                )
                .is_err()
        );
        if stop {
            assert!(result.is_err());
            assert!(!f.root.path().join("file.txt").exists());
            drop(j);
            j = f.journal();
            let observed = j
                .inspect_patch_recovery("alice", "op", &f.read)
                .await
                .expect("reopened inspection");
            assert!(matches!(
                observed.snapshot().expect("snapshot").original,
                RecoveryFileObservation::Regular {
                    reviewed_original_inode: true,
                    matches_original_content: true,
                    ..
                }
            ));
            drop(observed);
            let grant = j
                .authorize_patch_recovery("alice", "policy", "op", f.root.path())
                .expect("new write consent");
            let review = j
                .prepare_patch_resolution(
                    &grant,
                    &f.read,
                    "restore-native-original",
                    PatchRecoveryChoice::RestoreOriginalToMissingTarget,
                )
                .await
                .expect("actual restore preview");
            j.approve_patch_resolution(
                &grant,
                &f.read,
                &review.recovery_id,
                &review.digest,
                "alice",
                now().expect("time") + 60,
            )
            .await
            .expect("explicit approval");
            let token = j
                .dispatch_patch_resolution(&grant, &f.read, &review.recovery_id)
                .await
                .expect("dispatch recovery");
            j.publish_patch_resolution(&grant, &f.read, token)
                .expect("restore labelled original");
            assert_eq!(
                fs::read(f.root.path().join("file.txt")).expect("restored"),
                b"old\n"
            );
            assert_eq!(
                j.state("alice", "op").expect("uncertainty").as_deref(),
                Some("unknown")
            );
        } else {
            result.expect("publish with explicit native witness");
            drop(j);
            j = f.journal();
            let historical = j
                .revalidate_workspace_history("alice", "run", "source-before-capture", &f.read)
                .await
                .expect("actual retained identity validates history");
            assert_eq!(
                serde_json::to_value(historical.result()).expect("result"),
                prior
            );
            drop(historical);
            label(
                &f.root.path().join(&plan.backup_name),
                b"0082;65000001;NativeFixture;",
            );
            assert!(
                j.revalidate_workspace_history("alice", "run", "source-before-capture", &f.read)
                    .await
                    .is_err(),
                "changed retained security never becomes historical context"
            );
        }
    }
}
#[tokio::test]
async fn missing_capture_witness_and_other_permission_changes_fail_closed() {
    for fault in ["missing-witness", "mode", "label-authority"] {
        let f = Fixture::new();
        let (mut j, edit, token) = ready(&f).await;
        let plan = token.plan().clone();
        let result = j.publish_patch_with_hook(&edit, &f.read, token, |stage| {
            if stage == 1 && fault != "missing-witness" {
                label(
                    &f.root.path().join(&plan.staged_name),
                    b"0086;65000000;NativeFixture;",
                );
            }
            if stage == 3 {
                label(
                    &f.root.path().join(&plan.backup_name),
                    if fault == "label-authority" {
                        b"0082;65000001;NativeFixture;"
                    } else {
                        b"0086;65000001;NativeFixture;"
                    },
                );
                if fault == "mode" {
                    fs::set_permissions(
                        f.root.path().join(&plan.backup_name),
                        fs::Permissions::from_mode(0o777),
                    )
                    .expect("owned mode change");
                }
            }
            Ok(())
        });
        assert!(result.is_err());
        assert!(!f.root.path().join("file.txt").exists());
        assert_eq!(
            fs::read(f.root.path().join(&plan.backup_name)).expect("original retained"),
            b"old\n"
        );
        let snapshot = j
            .inspect_patch_recovery("alice", "op", &f.read)
            .await
            .expect("read-only facts");
        assert!(matches!(
            snapshot.snapshot().expect("snapshot").original,
            RecoveryFileObservation::Regular {
                reviewed_original_inode: false,
                ..
            }
        ));
    }
}
