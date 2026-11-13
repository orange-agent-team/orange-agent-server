use super::*;
use crate::{ResponseAuthority, storage::private_temp};
use orange_domain::ExecutionIdentity;
use std::fs;
struct Fixture {
    private: tempfile::TempDir,
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            private: private_temp(),
            root: tempfile::tempdir().expect("workspace"),
        }
    }
    fn open(&self) -> Journal {
        Journal::open(&self.private.path().join("state.sqlite")).expect("journal")
    }
    fn grant(&self, j: &mut Journal) -> CreateFileGrant {
        j.authorize_file_creation(
            "alice",
            "explicit-create-consent",
            "policy",
            self.root.path(),
        )
        .expect("trusted explicit create consent")
    }
}
fn response(j: &Journal) {
    j.register_run("alice", "run", "policy").expect("run");
}
// Explicit trusted-adapter fixture. This is not a model-quality certificate.
fn bind_intent(
    j: &mut Journal,
    grant: &CreateFileGrant,
    op: &Operation<'_>,
    args: &CreateFileArgs,
) {
    let candidate = orange_protocol::model::ResponseCandidate {
        response_id: format!("response-{}", op.id),
        text: String::new(),
        tool_calls: vec![orange_protocol::model::ToolCall {
            id: op.id.into(),
            name: "write_file".into(),
            arguments: serde_json::to_value(args).expect("args"),
        }],
        native_usage: None,
        finish_reason: "tool_calls".into(),
    };
    let digest = request_digest("model-response-v1", &candidate).expect("response digest");
    let identity = ExecutionIdentity {
        connection_epoch: 1,
        fencing: 1,
        attempt: 1,
    };
    j.commit_response(
        op.owner,
        op.run,
        op.invocation,
        ResponseAuthority {
            current: identity,
            incoming: identity,
            complete: true,
        },
        &digest,
    )
    .expect("validated complete response fixture");
    j.connection
        .execute(
            "UPDATE responses SET body=? WHERE invocation=? AND body IS NULL",
            params![
                serde_json::to_string(&candidate).expect("response"),
                op.invocation
            ],
        )
        .expect("fixture body");
    j.prepare_file_creation(grant, op, args, op.id)
        .expect("durable exact preview");
}
fn operation<'a>(id: &'a str, digest: &'a str, grant: &'a CreateFileGrant) -> Operation<'a> {
    Operation {
        id,
        owner: "alice",
        run: "run",
        invocation: id,
        argument_digest: digest,
        tool_version: "write_file.v1",
        environment: grant.environment(),
        policy_version: "policy",
        requires_approval: true,
    }
}
fn approve(j: &mut Journal, grant: &CreateFileGrant, op: &Operation<'_>, args: &CreateFileArgs) {
    bind_intent(j, grant, op, args);
    j.approve_file_creation(grant, op, op.argument_digest, "alice", i64::MAX)
        .expect("explicit stored-preview approval");
}
fn args(path: &str, content: &str) -> CreateFileArgs {
    CreateFileArgs {
        path: path.into(),
        content: content.into(),
    }
}
#[tokio::test]
async fn create_file_requires_response_write_authority_and_exact_approval() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let input = args("new.txt", "actual content");
    let digest = input.digest().expect("digest");
    let op = operation("create-1", &digest, &grant);
    j.register_run("alice", "run", "policy").expect("run");
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Journal(JournalError::ResponseRequired))
    ));
    response(&j);
    bind_intent(&mut j, &grant, &op, &input);
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Journal(JournalError::ApprovalRequired))
    ));
    assert!(!f.root.path().join("new.txt").exists());
    approve(&mut j, &grant, &op, &input);
    assert!(matches!(
        j.create_file(&grant, &op, args("new.txt", "changed body"))
            .await,
        Err(CreateFileError::Workspace(WorkspaceError::Forbidden))
    ));
    let receipt = j
        .create_file(&grant, &op, input.clone())
        .await
        .expect("actual atomic creation");
    assert_eq!(
        fs::read_to_string(f.root.path().join("new.txt")).expect("actual bytes"),
        input.content
    );
    assert_eq!(
        receipt.sha256,
        hex::encode(Sha256::digest(input.content.as_bytes()))
    );
    assert_eq!(
        j.state("alice", op.id).expect("state").as_deref(),
        Some("succeeded")
    );
    fs::write(f.root.path().join("new.txt"), "later user edit").expect("explicit later change");
    assert_eq!(
        j.create_file(&grant, &op, input)
            .await
            .expect("historical receipt only"),
        receipt
    );
    assert_eq!(
        fs::read_to_string(f.root.path().join("new.txt")).expect("current file"),
        "later user edit"
    );
    let count: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.dispatching'",
            [],
            |r| r.get(0),
        )
        .expect("event count");
    assert_eq!(count, 1);
}
#[tokio::test]
async fn create_file_never_overwrites_and_keeps_unicode_paths_literal() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    fs::create_dir(f.root.path().join("中文")).expect("parent");
    fs::write(f.root.path().join("中文/你好.txt"), "original").expect("existing file");
    let input = args("中文/你好.txt", "replacement");
    let digest = input.digest().expect("digest");
    let op = operation("conflict", &digest, &grant);
    approve(&mut j, &grant, &op, &input);
    assert!(matches!(
        j.create_file(&grant, &op, input).await,
        Err(CreateFileError::AlreadyExists)
    ));
    assert_eq!(
        fs::read_to_string(f.root.path().join("中文/你好.txt")).expect("file"),
        "original"
    );
    assert_eq!(
        fs::read_dir(f.root.path().join("中文"))
            .expect("directory")
            .count(),
        1
    );
    let input = args("中文/新建.txt", "新内容");
    let digest = input.digest().expect("digest");
    let op = operation("unicode", &digest, &grant);
    approve(&mut j, &grant, &op, &input);
    let receipt = j
        .create_file(&grant, &op, input)
        .await
        .expect("Unicode creation");
    assert_eq!(receipt.path, "中文/新建.txt");
    assert_eq!(receipt.bytes, "9");
}
#[tokio::test]
async fn create_file_rejects_readonly_environment_protected_names_links_and_revocation() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let readonly = WorkspaceGrant::open(
        "alice",
        "explicit-create-consent",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("readonly grant");
    assert_ne!(readonly.environment(), grant.environment());
    let input = args("new.txt", "body");
    let digest = input.digest().expect("digest");
    let mut op = operation("readonly-confusion", &digest, &grant);
    op.environment = readonly.environment();
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Workspace(WorkspaceError::Forbidden))
    ));
    for path in [
        "../outside.txt",
        ".env",
        "client.Key",
        ".Kube/a.txt",
        ".orange-agent-tmp-user",
    ] {
        assert!(args(path, "body").validate().is_err());
    }
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), f.root.path().join("link"))
            .expect("malicious parent");
        let input = args("link/escape.txt", "body");
        let digest = input.digest().expect("digest");
        let op = operation("parent-link", &digest, &grant);
        approve(&mut j, &grant, &op, &input);
        assert!(j.create_file(&grant, &op, input).await.is_err());
        assert!(!outside.path().join("escape.txt").exists());
    }
    let op = operation("revoked", &digest, &grant);
    approve(&mut j, &grant, &op, &input);
    grant.request_revoke();
    assert!(matches!(
        j.create_file(&grant, &op, input).await,
        Err(CreateFileError::Workspace(WorkspaceError::Revoked))
    ));
    assert!(!f.root.path().join("new.txt").exists());
}
#[tokio::test]
async fn create_file_postpublication_journal_failure_is_unknown_and_never_replayed() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let input = args("created.txt", "first publication");
    let digest = input.digest().expect("digest");
    let op = operation("faulted", &digest, &grant);
    approve(&mut j, &grant, &op, &input);
    j.connection.execute_batch("CREATE TRIGGER fail_creation_receipt BEFORE UPDATE ON operations WHEN NEW.state='succeeded' BEGIN SELECT RAISE(ABORT,'explicit receipt fault');END;").expect("failure fixture");
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Unknown)
    ));
    assert_eq!(
        fs::read_to_string(f.root.path().join("created.txt")).expect("actual published effect"),
        input.content
    );
    drop(j);
    let mut j = f.open();
    assert_eq!(
        j.state("alice", "faulted").expect("recovery").as_deref(),
        Some("unknown")
    );
    fs::write(f.root.path().join("created.txt"), "user edit after crash").expect("user change");
    assert!(matches!(
        j.authorize_file_creation("alice", "explicit-create-consent", "policy", f.root.path()),
        Err(CreateFileError::Unknown)
    ));
    assert!(matches!(
        j.authorize_file_creation(
            "alice",
            "new-consent-cannot-bypass",
            "policy",
            f.root.path()
        ),
        Err(CreateFileError::Unknown)
    ));
    assert_eq!(
        fs::read_to_string(f.root.path().join("created.txt")).expect("preserve"),
        "user edit after crash"
    );
}

#[tokio::test]
async fn create_file_racing_hosts_publish_exactly_one_complete_file() {
    let f = Fixture::new();
    let second = private_temp();
    let mut a = f.open();
    let mut b = Journal::open(&second.path().join("state.sqlite")).expect("independent journal");
    response(&a);
    response(&b);
    let ga = f.grant(&mut a);
    let gb = f.grant(&mut b);
    let aa = args("race.txt", &"α".repeat(20_000));
    let ab = args("race.txt", &"β".repeat(20_000));
    let da = aa.digest().expect("digest");
    let db = ab.digest().expect("digest");
    let oa = operation("race-a", &da, &ga);
    let ob = operation("race-b", &db, &gb);
    approve(&mut a, &ga, &oa, &aa);
    approve(&mut b, &gb, &ob, &ab);
    let (ra, rb) = tokio::join!(
        a.create_file(&ga, &oa, aa.clone()),
        b.create_file(&gb, &ob, ab.clone())
    );
    assert_eq!(usize::from(ra.is_ok()) + usize::from(rb.is_ok()), 1);
    let expected = if ra.is_ok() {
        assert!(matches!(rb, Err(CreateFileError::AlreadyExists)));
        aa.content
    } else {
        assert!(matches!(ra, Err(CreateFileError::AlreadyExists)));
        ab.content
    };
    assert_eq!(
        fs::read_to_string(f.root.path().join("race.txt")).expect("published content"),
        expected
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no staging leftovers")
            .count(),
        1
    );
}
#[tokio::test]
async fn create_file_expired_or_optional_approval_never_writes() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let input = args("new.txt", "body");
    let digest = input.digest().expect("digest");
    let mut op = operation("expired", &digest, &grant);
    bind_intent(&mut j, &grant, &op, &input);
    j.approve_file_creation(&grant, &op, &digest, "alice", 0)
        .expect("expired fixture");
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Journal(JournalError::ApprovalRequired))
    ));
    op.requires_approval = false;
    assert!(matches!(
        j.create_file(&grant, &op, input).await,
        Err(CreateFileError::Workspace(WorkspaceError::Forbidden))
    ));
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("empty workspace")
            .count(),
        0
    );
}

#[tokio::test]
async fn create_file_digest_only_response_and_generic_approval_cannot_bypass_preview() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let input = args("new.txt", "bound content");
    let digest = input.digest().expect("digest");
    let op = operation("unbound", &digest, &grant);
    let id = ExecutionIdentity {
        connection_epoch: 1,
        fencing: 1,
        attempt: 1,
    };
    j.commit_response(
        "alice",
        "run",
        op.invocation,
        ResponseAuthority {
            current: id,
            incoming: id,
            complete: true,
        },
        &"a".repeat(64),
    )
    .expect("legacy digest-only response");
    j.prepare(&op).expect("generic intent");
    j.approve(&op, "alice", i64::MAX).expect("generic approval");
    assert!(matches!(
        j.create_file(&grant, &op, input).await,
        Err(CreateFileError::Journal(JournalError::ResponseRequired))
    ));
    assert_eq!(
        j.state("alice", op.id).expect("state").as_deref(),
        Some("intended")
    );
    assert_eq!(
        fs::read_dir(f.root.path())
            .expect("no filesystem work")
            .count(),
        0
    );
}
#[tokio::test]
async fn create_file_preview_reopens_exactly_and_rejects_changed_response_or_digest() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let input = args("新文件.txt", "reviewed content\n");
    let digest = input.digest().expect("digest");
    let op = operation("preview", &digest, &grant);
    bind_intent(&mut j, &grant, &op, &input);
    let original = j
        .file_creation_preview(&grant, &op)
        .expect("stored preview");
    assert_eq!(original.arguments, input);
    assert_eq!(original.call_id, "preview");
    assert!(
        j.approve_file_creation(&grant, &op, &"0".repeat(64), "alice", i64::MAX)
            .is_err()
    );
    drop(j);
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let op = operation("preview", &digest, &grant);
    assert_eq!(
        j.file_creation_preview(&grant, &op)
            .expect("same committed body"),
        original
    );
    let saved: String = j
        .connection
        .query_row(
            "SELECT body FROM responses WHERE invocation=?",
            [op.invocation],
            |r| r.get(0),
        )
        .expect("body");
    let mut changed: serde_json::Value = serde_json::from_str(&saved).expect("fixture");
    changed["tool_calls"][0]["arguments"]["content"] = json!("not reviewed");
    j.connection
        .execute(
            "UPDATE responses SET body=? WHERE invocation=?",
            params![changed.to_string(), op.invocation],
        )
        .expect("explicit storage fault");
    assert!(j.file_creation_preview(&grant, &op).is_err());
    assert!(
        j.approve_file_creation(&grant, &op, &digest, "alice", i64::MAX)
            .is_err()
    );
    assert_eq!(fs::read_dir(f.root.path()).expect("unchanged").count(), 0);
    j.connection
        .execute(
            "UPDATE responses SET body=? WHERE invocation=?",
            params![saved, op.invocation],
        )
        .expect("restore test fixture");
    j.approve_file_creation(&grant, &op, &digest, "alice", i64::MAX)
        .expect("approve original");
    j.create_file(&grant, &op, input)
        .await
        .expect("execute original after reopen");
}
#[tokio::test]
async fn create_file_wrong_call_foreign_response_or_forged_stored_preview_has_no_effect() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let grant = f.grant(&mut j);
    let input = args("new.txt", "content");
    let digest = input.digest().expect("digest");
    let op = operation("source", &digest, &grant);
    bind_intent(&mut j, &grant, &op, &input);
    assert!(
        j.prepare_file_creation(&grant, &op, &input, "different-call")
            .is_err()
    );
    j.register_run("alice", "foreign-run", "policy")
        .expect("foreign run fixture");
    j.connection
        .execute(
            "UPDATE responses SET run_id='foreign-run' WHERE invocation=?",
            [op.invocation],
        )
        .expect("foreign binding fault");
    assert!(j.file_creation_preview(&grant, &op).is_err());
    j.connection
        .execute(
            "UPDATE responses SET run_id='run' WHERE invocation=?",
            [op.invocation],
        )
        .expect("restore fixture");
    j.connection.execute("UPDATE operations SET request_json=? WHERE operation_id=?",params![json!({"tool":"write_file","arguments":{"path":"different.txt","content":"content"}}).to_string(),op.id]).expect("preview corruption fixture");
    assert!(j.file_creation_preview(&grant, &op).is_err());
    assert!(j.create_file(&grant, &op, input).await.is_err());
    assert_eq!(fs::read_dir(f.root.path()).expect("no effect").count(), 0);
    let dispatched: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM events WHERE kind='operation.dispatching'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(dispatched, 0);
}

#[test]
fn create_consent_is_owned_idempotent_and_does_not_upgrade_readonly_records() {
    let f = Fixture::new();
    let mut j = f.open();
    let read = j
        .authorize_workspace("alice", "read-consent", f.root.path(), "policy")
        .expect("readonly consent");
    assert!(
        j.creation_consents("alice")
            .expect("no implicit writes")
            .is_empty()
    );
    let first = f.grant(&mut j);
    let id = first.consent_id().to_owned();
    let again = f.grant(&mut j);
    assert_eq!(first.environment(), again.environment());
    assert_eq!(id, again.consent_id());
    assert!(j.creation_consent("bob", &id).is_err());
    let other = tempfile::tempdir().expect("other root");
    assert!(matches!(
        j.authorize_file_creation("alice", "explicit-create-consent", "policy", other.path()),
        Err(CreateFileError::Journal(JournalError::Conflict))
    ));
    assert_eq!(j.creation_consents("alice").expect("inventory").len(), 1);
    assert_eq!(
        j.workspace("alice", &read.workspace_id)
            .expect("original readonly view")
            .environment,
        read.environment
    );
    assert_ne!(read.environment, first.environment());
    assert!(
        j.authorize_file_creation("alice", "private-write", "policy", f.private.path())
            .is_err()
    );
}
#[tokio::test]
async fn create_consent_revoke_closes_all_handles_and_old_request_cannot_reactivate() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let first = f.grant(&mut j);
    let id = first.consent_id().to_owned();
    let second = j
        .restore_file_creation("alice", &id)
        .expect("second current handle");
    assert!(matches!(
        j.revoke_file_creation("alice", &id, "2"),
        Err(CreateFileError::Journal(JournalError::Conflict))
    ));
    assert!(!first.grant.is_revoked());
    let signal = j.creation_revoker("alice", &id).expect("owned signal");
    signal.request_revoke();
    assert!(first.grant.is_revoked() && second.grant.is_revoked());
    assert!(
        j.creation_consent("alice", &id)
            .expect("signal not durable acknowledgement")
            .active
    );
    let stopped = j
        .revoke_file_creation("alice", &id, "1")
        .expect("durable revoke");
    assert!(!stopped.active);
    assert_eq!(stopped.version, "2");
    assert_eq!(
        j.revoke_file_creation("alice", &id, "1")
            .expect("same revoke replay"),
        stopped
    );
    assert!(matches!(
        j.authorize_file_creation("alice", "explicit-create-consent", "policy", f.root.path()),
        Err(CreateFileError::Workspace(WorkspaceError::Revoked))
    ));
    let fresh = j
        .authorize_file_creation("alice", "fresh-explicit-consent", "policy", f.root.path())
        .expect("new independent user consent");
    assert_ne!(fresh.environment(), first.environment());
    let input = args("after.txt", "content");
    let hash = input.digest().expect("digest");
    let old = operation("late-old", &hash, &first);
    assert!(matches!(
        j.create_file(&first, &old, input).await,
        Err(CreateFileError::Workspace(WorkspaceError::Revoked))
    ));
    assert!(!f.root.path().join("after.txt").exists());
}
#[test]
fn create_consent_reopen_needs_fresh_capability_and_retains_exact_preview() {
    let f = Fixture::new();
    let mut j = f.open();
    response(&j);
    let old = f.grant(&mut j);
    let id = old.consent_id().to_owned();
    let input = args("new.txt", "content");
    let hash = input.digest().expect("digest");
    let op = operation("resume", &hash, &old);
    bind_intent(&mut j, &old, &op, &input);
    drop(j);
    let mut j = f.open();
    assert!(
        j.creation_consent("alice", &id)
            .expect("stored consent")
            .active
    );
    assert!(
        j.file_creation_preview(&old, &op).is_err(),
        "prior Host handle never grants new-session write access"
    );
    let current = j
        .restore_file_creation("alice", &id)
        .expect("trusted Host explicitly restored access");
    let op = operation("resume", &hash, &current);
    assert_eq!(
        j.file_creation_preview(&current, &op)
            .expect("same durable preview")
            .arguments,
        input
    );
}
#[test]
fn create_consent_replaced_directory_and_private_area_overlap_are_refused() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let id = grant.consent_id().to_owned();
    let inside = f.root.path().join("future-private");
    fs::create_dir(&inside).expect("private area fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&inside, fs::Permissions::from_mode(0o700)).expect("private");
    }
    assert!(
        j.reserve_private_directory(&inside).is_err(),
        "must not create a secret namespace under an active writer"
    );
    let moved = f
        .root
        .path()
        .with_extension(format!("moved-{}", uuid::Uuid::new_v4()));
    fs::rename(f.root.path(), &moved).expect("move exact owned fixture");
    fs::create_dir(f.root.path()).expect("replace path");
    assert!(matches!(
        j.restore_file_creation("alice", &id),
        Err(CreateFileError::Workspace(
            WorkspaceError::EnvironmentChanged
        ))
    ));
    assert!(
        !j.creation_consent("alice", &id)
            .expect("fenced consent")
            .active
    );
    assert!(grant.grant.is_revoked());
    fs::remove_dir_all(moved).expect("remove only moved owned fixture");
}
#[test]
fn create_consent_migration_preserves_legacy_data_without_inventing_write_rights() {
    let f = Fixture::new();
    let mut j = f.open();
    let read = j
        .authorize_workspace("alice", "prior-read", f.root.path(), "policy")
        .expect("readonly");
    j.connection.execute_batch("DROP TABLE patch_version_edges; DROP TABLE patch_retention_archives; DROP TABLE patch_retention_reviews; DROP TABLE patch_recovery_resolutions; DROP TABLE patch_recovery_results; DROP TABLE patch_recovery_approvals; DROP TABLE patch_recoveries; DROP TABLE editing_publication_steps; DROP TABLE editing_publications; DROP TABLE editing_approvals; DROP TABLE editing_previews; DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; DROP TABLE runtime_tool_approvals; DROP TABLE runtime_tool_catalogs; DROP TABLE runtime_catalog_legacy_approvals; DROP TABLE runtime_catalog_legacy; DROP TABLE runtime_creation_capabilities; DROP TABLE creation_consent_audit;DROP TABLE creation_request_keys;DROP TABLE creation_consents;PRAGMA user_version=4;").expect("real prior schema fixture");
    drop(j);
    let j = f.open();
    assert!(
        j.creation_consents("alice")
            .expect("no implicit writes")
            .is_empty()
    );
    assert_eq!(
        j.workspace("alice", &read.workspace_id)
            .expect("preserved readonly environment")
            .environment,
        read.environment
    );
    let broken: i64 = j
        .connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .expect("FK integrity");
    assert_eq!(broken, 0);
}

#[test]
fn create_consent_revocation_failure_does_not_acknowledge_or_restore_live_handles() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let id = grant.consent_id().to_owned();
    j.connection.execute_batch("CREATE TRIGGER reject_consent_revoke BEFORE INSERT ON creation_consent_audit WHEN NEW.kind='creation.revoked' BEGIN SELECT RAISE(ABORT,'explicit revoke audit failure');END;").expect("fault fixture");
    assert!(j.revoke_file_creation("alice", &id, "1").is_err());
    assert!(grant.grant.is_revoked());
    let stored = j
        .creation_consent("alice", &id)
        .expect("rolled back durable state");
    assert!(stored.active);
    assert_eq!(stored.version, "1");
    assert!(matches!(
        j.restore_file_creation("alice", &id),
        Err(CreateFileError::Workspace(WorkspaceError::Revoked))
    ));
    j.connection
        .execute_batch("DROP TRIGGER reject_consent_revoke")
        .expect("remove owned fault");
    assert!(
        !j.revoke_file_creation("alice", &id, "1")
            .expect("retry original revocation")
            .active
    );
}
#[test]
fn create_consent_terminal_identity_keys_and_history_are_immutable() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let id = grant.consent_id().to_owned();
    assert!(
        j.connection
            .execute(
                "UPDATE creation_consents SET policy='changed' WHERE consent_id=?",
                [&id]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM creation_consents WHERE consent_id=?", [&id])
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "UPDATE creation_request_keys SET digest='changed' WHERE consent_id=?",
                [&id]
            )
            .is_err()
    );
    assert!(
        j.connection
            .execute(
                "DELETE FROM creation_consent_audit WHERE consent_id=?",
                [&id]
            )
            .is_err()
    );
    j.revoke_file_creation("alice", &id, "1").expect("revoke");
    assert!(
        j.connection
            .execute(
                "UPDATE creation_consents SET active=1,version=3 WHERE consent_id=?",
                [&id]
            )
            .is_err()
    );
}

#[test]
fn create_consent_request_aliases_are_bounded_but_exact_retry_still_resolves() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = f.grant(&mut j);
    let id = grant.consent_id().to_owned();
    let tx = j
        .connection
        .transaction()
        .expect("quota fixture transaction");
    for i in 0..8191 {
        tx.execute("INSERT INTO creation_request_keys(owner,request_id,digest,consent_id) VALUES('alice',?,'quota-fixture',?)",params![format!("alias-{i}"),id]).expect("owned quota fixture");
    }
    tx.commit().expect("quota fixture");
    assert!(matches!(
        j.authorize_file_creation("alice", "extra-alias", "policy", f.root.path()),
        Err(CreateFileError::Workspace(WorkspaceError::Limit))
    ));
    assert_eq!(
        f.grant(&mut j).consent_id(),
        id,
        "lost-response retry must not consume a new key"
    );
    assert_eq!(
        j.creation_consents("alice")
            .expect("no invented extra grants")
            .len(),
        1
    );
}

#[tokio::test]
async fn create_publication_observes_read_revocation_and_holds_both_drain_leases() {
    let f = Fixture::new();
    let mut j = f.open();
    let create = f.grant(&mut j);
    let read = WorkspaceGrant::open(
        "alice",
        "read",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("read grant");
    let write_permit = create
        .grant
        .permit("alice", create.grant.environment(), "policy")
        .await
        .expect("write permit");
    let read_permit = read
        .permit("alice", read.environment(), "policy")
        .await
        .expect("read permit");
    read.request_revoke();
    assert!(matches!(
        publish(
            write_permit,
            Some(read_permit),
            args("forbidden.txt", "never published")
        ),
        Err(CreateFileError::Workspace(WorkspaceError::Revoked))
    ));
    assert!(!f.root.path().join("forbidden.txt").exists());
    read.wait_drained().await;
    assert!(
        !create.grant.is_revoked(),
        "read revocation does not rewrite separate write consent"
    );
    let read = WorkspaceGrant::open(
        "alice",
        "fresh-read",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("fresh read grant");
    let write_permit = create
        .grant
        .permit("alice", create.grant.environment(), "policy")
        .await
        .expect("write permit");
    let read_permit = read
        .permit("alice", read.environment(), "policy")
        .await
        .expect("read permit");
    let receipt = publish(
        write_permit,
        Some(read_permit),
        args("created.txt", "actual bytes"),
    )
    .expect("publish");
    assert_eq!(
        fs::read_to_string(f.root.path().join("created.txt")).expect("file"),
        "actual bytes"
    );
    read.request_revoke();
    create.request_revoke();
    assert!(read.is_draining());
    assert!(create.grant.is_draining());
    drop(receipt);
    read.wait_drained().await;
    create.wait_drained().await;
    assert!(!read.is_draining());
    assert!(!create.grant.is_draining());
}
