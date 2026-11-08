use super::*;
use crate::{ResponseAuthority, storage::private_temp};
use orange_domain::ExecutionIdentity;
use orange_protocol::model::ToolCall;
use std::fs;
struct Fixture {
    private: tempfile::TempDir,
    root: tempfile::TempDir,
    read: WorkspaceGrant,
    args: PatchArgs,
}
impl Fixture {
    fn new() -> Self {
        let private = private_temp();
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("file.txt"), b"old\n").expect("source");
        let read = WorkspaceGrant::open(
            "alice",
            "read",
            "policy",
            root.path(),
            &[private.path().into()],
        )
        .expect("read");
        let args = PatchArgs {
            path: "file.txt".into(),
            expected_sha256: hex::encode(sha2::Sha256::digest(b"old\n")),
            patch: "@@ -1 +1 @@\n-old\n+new\n".into(),
        };
        Self {
            private,
            root,
            read,
            args,
        }
    }
    fn journal(&self) -> Journal {
        Journal::open(&self.private.path().join("journal.sqlite")).expect("journal")
    }
    fn edit(&self, j: &mut Journal) -> EditFileGrant {
        j.authorize_file_edit("alice", "explicit-edit", "policy", self.root.path())
            .expect("edit consent")
    }
    fn response(&self, j: &Journal, body: bool) {
        j.register_run("alice", "run", "policy").expect("run");
        let candidate = ResponseCandidate {
            response_id: "response".into(),
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "patch-call".into(),
                name: "apply_patch".into(),
                arguments: serde_json::to_value(&self.args).expect("args"),
            }],
            native_usage: None,
            finish_reason: "tool_calls".into(),
        };
        let digest = request_digest("model-response-v1", &candidate).expect("digest");
        let identity = ExecutionIdentity {
            connection_epoch: 1,
            fencing: 1,
            attempt: 1,
        };
        j.commit_response(
            "alice",
            "run",
            "inv",
            ResponseAuthority {
                current: identity,
                incoming: identity,
                complete: true,
            },
            &digest,
        )
        .expect("explicit trusted adapter fixture");
        if body {
            j.connection
                .execute(
                    "UPDATE responses SET body=? WHERE invocation='inv'",
                    [serde_json::to_string(&candidate).expect("body")],
                )
                .expect("committed body fixture");
        }
    }
}
fn op<'a>(edit: &'a EditFileGrant, digest: &'a str) -> Operation<'a> {
    Operation {
        id: "op",
        owner: "alice",
        run: "run",
        invocation: "inv",
        argument_digest: digest,
        tool_version: "apply_patch.v1",
        environment: edit.environment(),
        policy_version: "policy",
        requires_approval: true,
    }
}
fn count(j: &Journal, table: &str) -> i64 {
    j.connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

async fn approved_fixture(
    f: &Fixture,
    j: &mut Journal,
    edit: &EditFileGrant,
    operation: &Operation<'_>,
) {
    let review = j
        .prepare_patch_review(edit, &f.read, operation, f.args.clone(), "patch-call")
        .await
        .expect("review");
    j.approve_patch_review(
        edit,
        &f.read,
        operation,
        &review.preview_digest,
        "alice",
        now().expect("clock") + 300,
    )
    .await
    .expect("approval");
}

#[tokio::test]
async fn publication_reservation_is_durable_owner_scoped_and_never_replayed_after_reopen() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    let token = j
        .begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("durable barrier");
    assert!(token.has_live_directory_access());
    let plan = token.plan().clone();
    assert_ne!(plan.staged_name, plan.backup_name);
    assert!(components(&plan.staged_name, false).is_err());
    assert!(components(&plan.backup_name, false).is_err());
    assert_eq!(
        j.patch_publication_plan("alice", "op").expect("owner"),
        Some(plan.clone())
    );
    assert_eq!(
        j.patch_publication_plan("bob", "op")
            .expect("foreign owner"),
        None
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("dispatching")
    );
    assert!(
        j.begin_patch_publication(&edit, &f.read, &operation)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no filesystem effect")
            .count(),
        1
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
    drop(token);
    drop(j);
    let mut j = f.journal();
    assert_eq!(
        j.state("alice", "op").expect("recovered").as_deref(),
        Some("unknown")
    );
    assert_eq!(
        j.patch_publication_plan("alice", "op")
            .expect("recovery evidence"),
        Some(plan)
    );
    assert!(
        j.begin_patch_publication(&edit, &f.read, &operation)
            .await
            .is_err()
    );
    assert_eq!(count(&j, "editing_publications"), 1);
    assert!(
        j.connection
            .execute("DELETE FROM editing_publications", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("UPDATE editing_publications SET reserved_bytes=1", [])
            .is_err()
    );
}

#[tokio::test]
async fn reservation_failure_rolls_back_dispatch_and_preserves_original_approval() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    j.connection.execute_batch("CREATE TRIGGER fail_reservation BEFORE INSERT ON editing_publications BEGIN SELECT RAISE(ABORT,'injected reservation failure');END;").expect("fault");
    assert!(
        j.begin_patch_publication(&edit, &f.read, &operation)
            .await
            .is_err()
    );
    assert_eq!(
        j.state("alice", "op").expect("rolled back").as_deref(),
        Some("intended")
    );
    assert_eq!(count(&j, "editing_publications"), 0);
    assert_eq!(count(&j, "editing_approvals"), 1);
    j.connection
        .execute_batch("DROP TRIGGER fail_reservation;")
        .expect("remove owned injection");
    j.begin_patch_publication(&edit, &f.read, &operation)
        .await
        .expect("same exact approval accepted once");
    assert_eq!(count(&j, "editing_publications"), 1);
}

#[tokio::test]
async fn changed_source_or_revoked_access_cannot_cross_publication_barrier() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    approved_fixture(&f, &mut j, &edit, &operation).await;
    fs::write(f.root.path().join("file.txt"), b"external change\n").expect("external write");
    assert!(
        j.begin_patch_publication(&edit, &f.read, &operation)
            .await
            .is_err()
    );
    assert_eq!(count(&j, "editing_publications"), 0);
    edit.request_revoke();
    assert!(
        j.begin_patch_publication(&edit, &f.read, &operation)
            .await
            .is_err()
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("intended")
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("preserved"),
        b"external change\n"
    );
}

#[tokio::test]
async fn retained_publications_have_a_bounded_per_resource_admission_limit() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    for n in 0..65 {
        let id = format!("operation-{n}");
        let invocation = format!("invocation-{n}");
        j.connection.execute("INSERT INTO responses(invocation,run_id,attempt,digest,body) SELECT ?,run_id,attempt,digest,body FROM responses WHERE invocation='inv'",[&invocation]).expect("distinct committed response fixture");
        let operation = Operation {
            id: &id,
            invocation: &invocation,
            ..op(&edit, &digest)
        };
        approved_fixture(&f, &mut j, &edit, &operation).await;
        let outcome = j.begin_patch_publication(&edit, &f.read, &operation).await;
        if n < 64 {
            assert!(outcome.is_ok());
            // Journal-only terminal fixture for retained-plan accounting;
            // this does not certify a filesystem publication.
            j.complete("alice", &id, &digest)
                .expect("synthetic terminal journal fixture");
        } else {
            assert!(matches!(outcome, Err(PatchError::Limit)));
            assert_eq!(
                j.state("alice", &id).expect("not dispatched").as_deref(),
                Some("intended")
            );
        }
    }
    assert_eq!(count(&j, "editing_publications"), 64);
    assert_eq!(fs::read_dir(f.root.path()).expect("no effects").count(), 1);
}

#[tokio::test]
async fn retention_byte_limit_applies_before_the_plan_count_limit() {
    let mut f = Fixture::new();
    let content = format!("old\n{}\n", "a".repeat(MAX_TEXT - 5));
    fs::write(f.root.path().join("file.txt"), &content).expect("large owned text");
    f.args.expected_sha256 = hex::encode(sha2::Sha256::digest(content.as_bytes()));
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let expected_admissions = (64 * 1024 * 1024) / (2 * MAX_TEXT + 2 * 384 * 1024);
    assert!(expected_admissions < 64);
    for n in 0..=expected_admissions {
        let id = format!("operation-{n}");
        let invocation = format!("invocation-{n}");
        j.connection.execute("INSERT INTO responses(invocation,run_id,attempt,digest,body) SELECT ?,run_id,attempt,digest,body FROM responses WHERE invocation='inv'",[&invocation]).expect("distinct committed response fixture");
        let operation = Operation {
            id: &id,
            invocation: &invocation,
            ..op(&edit, &digest)
        };
        approved_fixture(&f, &mut j, &edit, &operation).await;
        let result = j.begin_patch_publication(&edit, &f.read, &operation).await;
        if n < expected_admissions {
            assert!(result.is_ok());
            j.complete("alice", &id, &digest)
                .expect("synthetic terminal journal fixture");
        } else {
            assert!(matches!(result, Err(PatchError::Limit)));
            assert_eq!(
                j.state("alice", &id).expect("state").as_deref(),
                Some("intended")
            );
        }
    }
    assert_eq!(
        count(&j, "editing_publications"),
        expected_admissions as i64
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("file.txt")).expect("unchanged"),
        content
    );
}
#[tokio::test]
async fn exact_review_and_separate_approval_never_publish_a_file_by_themselves() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, false);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    assert!(
        j.prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
            .await
            .is_err()
    );
    assert_eq!(count(&j, "editing_previews"), 0);
    let body = ResponseCandidate {
        response_id: "response".into(),
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "patch-call".into(),
            name: "apply_patch".into(),
            arguments: serde_json::to_value(&f.args).expect("args"),
        }],
        native_usage: None,
        finish_reason: "tool_calls".into(),
    };
    j.connection
        .execute(
            "UPDATE responses SET body=?",
            [serde_json::to_string(&body).expect("body")],
        )
        .expect("full body fixture");
    let review = j
        .prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
        .await
        .expect("persisted actual review");
    assert_eq!(
        j.patch_review(&edit, &f.read, &operation)
            .await
            .expect("reload"),
        review
    );
    j.approve(&operation, "alice", now().expect("time") + 60)
        .expect("generic approval");
    assert!(j.approved_patch(&edit, &f.read, &operation).await.is_err());
    assert!(
        j.approve_patch_review(
            &edit,
            &f.read,
            &operation,
            &"0".repeat(64),
            "alice",
            now().expect("time") + 60
        )
        .await
        .is_err()
    );
    assert_eq!(count(&j, "editing_approvals"), 0);
    j.approve_patch_review(
        &edit,
        &f.read,
        &operation,
        &review.preview_digest,
        "alice",
        now().expect("time") + 60,
    )
    .await
    .expect("exact preview approval");
    let ready = j
        .approved_patch(&edit, &f.read, &operation)
        .await
        .expect("prepared evidence");
    assert!(ready.has_live_directory_access());
    assert_eq!(ready.content(), b"new\n");
    assert_eq!(ready.review(), &review);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no publication"),
        b"old\n"
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("intended")
    );
}
#[tokio::test]
async fn persisted_review_reopens_exactly_but_stale_capabilities_are_not_restored() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let review = j
        .prepare_patch_review(
            &edit,
            &f.read,
            &op(&edit, &digest),
            f.args.clone(),
            "patch-call",
        )
        .await
        .expect("review");
    let id = edit.consent_id().to_owned();
    drop(j);
    let mut j = f.journal();
    assert!(
        j.patch_review(&edit, &f.read, &op(&edit, &digest))
            .await
            .is_err()
    );
    let fresh = j
        .restore_file_edit("alice", &id)
        .expect("fresh OS access fixture");
    assert_eq!(
        j.patch_review(&fresh, &f.read, &op(&fresh, &digest))
            .await
            .expect("restored"),
        review
    );
    j.approve_patch_review(
        &fresh,
        &f.read,
        &op(&fresh, &digest),
        &review.preview_digest,
        "alice",
        now().expect("time") + 60,
    )
    .await
    .expect("approve");
    drop(j);
    let mut j = f.journal();
    let newest = j.restore_file_edit("alice", &id).expect("fresh capability");
    let ready = j
        .approved_patch(&newest, &f.read, &op(&newest, &digest))
        .await
        .expect("same approved bytes");
    assert_eq!(ready.content(), b"new\n");
    assert_eq!(count(&j, "editing_previews"), 1);
    assert_eq!(count(&j, "editing_approvals"), 1);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
}
#[tokio::test]
async fn changed_source_before_or_after_approval_never_rebases_or_reexposes_preview() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    let review = j
        .prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
        .await
        .expect("review");
    fs::write(f.root.path().join("file.txt"), b"external\n").expect("external fixture edit");
    assert!(j.patch_review(&edit, &f.read, &operation).await.is_err());
    assert!(
        j.approve_patch_review(
            &edit,
            &f.read,
            &operation,
            &review.preview_digest,
            "alice",
            now().expect("time") + 60
        )
        .await
        .is_err()
    );
    assert_eq!(count(&j, "approvals"), 0);
    fs::write(f.root.path().join("file.txt"), b"old\n")
        .expect("same bytes but changed identity metadata");
    assert!(
        j.prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
            .await
            .is_err(),
        "old operation cannot silently replace its reviewed source binding"
    );
    assert_eq!(count(&j, "editing_previews"), 1);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("preserved"),
        b"old\n"
    );
}
#[tokio::test]
async fn preview_and_approval_failures_roll_back_their_bound_records() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    j.connection.execute_batch("CREATE TRIGGER fail_preview BEFORE INSERT ON editing_previews BEGIN SELECT RAISE(ABORT,'injected fault');END;").expect("fault");
    assert!(
        j.prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
            .await
            .is_err()
    );
    assert_eq!(count(&j, "editing_previews"), 0);
    assert!(
        j.connection
            .query_row("SELECT request_json FROM operations", [], |r| r
                .get::<_, Option<String>>(0))
            .expect("unbound intent")
            .is_none()
    );
    j.connection
        .execute_batch("DROP TRIGGER fail_preview;")
        .expect("repair");
    let review = j
        .prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
        .await
        .expect("review");
    j.connection.execute_batch("CREATE TRIGGER fail_patch_approval BEFORE INSERT ON editing_approvals BEGIN SELECT RAISE(ABORT,'injected fault');END;").expect("fault");
    assert!(
        j.approve_patch_review(
            &edit,
            &f.read,
            &operation,
            &review.preview_digest,
            "alice",
            now().expect("time") + 60
        )
        .await
        .is_err()
    );
    assert_eq!(count(&j, "approvals"), 0);
    assert_eq!(count(&j, "editing_approvals"), 0);
    assert!(
        j.connection
            .execute("UPDATE editing_previews SET digest=?", ["a".repeat(64)])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM editing_previews", [])
            .is_err()
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no effect"),
        b"old\n"
    );
}
#[tokio::test]
async fn changed_response_wrong_call_revocation_and_expiry_refuse_approval() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    assert!(
        j.prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "foreign-call")
            .await
            .is_err()
    );
    let review = j
        .prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
        .await
        .expect("review");
    assert!(
        j.approve_patch_review(
            &edit,
            &f.read,
            &operation,
            &review.preview_digest,
            "alice",
            now().expect("time") - 1
        )
        .await
        .is_err()
    );
    j.approve_patch_review(
        &edit,
        &f.read,
        &operation,
        &review.preview_digest,
        "alice",
        now().expect("time") + 60,
    )
    .await
    .expect("approve");
    j.connection
        .execute("UPDATE approvals SET expires_at=0", [])
        .expect("expire fixture");
    assert!(j.approved_patch(&edit, &f.read, &operation).await.is_err());
    j.connection
        .execute("UPDATE responses SET digest=?", ["a".repeat(64)])
        .expect("corrupt source fixture");
    assert!(j.patch_review(&edit, &f.read, &operation).await.is_err());
    j.revoke_file_edit(
        "alice",
        edit.consent_id(),
        &edit.consent_version().expect("version"),
    )
    .expect("revoke");
    assert!(j.approved_patch(&edit, &f.read, &operation).await.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no effect"),
        b"old\n"
    );
}

#[tokio::test]
async fn approved_review_refuses_later_external_changes_and_keeps_their_bytes() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    f.response(&j, true);
    let digest = f.args.digest().expect("digest");
    let operation = op(&edit, &digest);
    let review = j
        .prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
        .await
        .expect("review");
    j.approve_patch_review(
        &edit,
        &f.read,
        &operation,
        &review.preview_digest,
        "alice",
        now().expect("time") + 60,
    )
    .await
    .expect("approval");
    fs::write(f.root.path().join("file.txt"), b"external newer version\n")
        .expect("external modification");
    assert!(j.approved_patch(&edit, &f.read, &operation).await.is_err());
    assert!(j.patch_review(&edit, &f.read, &operation).await.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("external content retained"),
        b"external newer version\n"
    );
    assert_eq!(
        j.state("alice", "op").expect("state").as_deref(),
        Some("intended")
    );
    assert_eq!(count(&j, "editing_approvals"), 1);
}
#[tokio::test]
async fn low_level_patch_review_does_not_enable_unregistered_normal_agent_tools() {
    use orange_protocol::model::{Message, Role};
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    let config = crate::engine::RunConfig {
        owner: "alice".into(),
        run_id: "normal".into(),
        policy: "policy".into(),
        environment: f.read.environment().into(),
        model_version: "fixture".into(),
        max_steps: 8,
        max_invocations: 4,
        max_output_tokens: 128,
        deadline_unix: now().expect("time") + 120,
    };
    j.start_agent(
        &config,
        vec![Message::plain(
            Role::User,
            "Do not silently add editing".into(),
        )],
        &f.read,
    )
    .expect("normal readonly Run");
    let digest = f.args.digest().expect("digest");
    let mut operation = op(&edit, &digest);
    operation.run = "normal";
    assert!(matches!(
        j.prepare_patch_review(&edit, &f.read, &operation, f.args.clone(), "patch-call")
            .await,
        Err(PatchError::RecoveryRequired)
    ));
    assert_eq!(count(&j, "editing_previews"), 0);
    assert_eq!(count(&j, "approvals"), 0);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
}
