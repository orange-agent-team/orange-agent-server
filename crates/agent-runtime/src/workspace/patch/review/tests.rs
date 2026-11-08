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
