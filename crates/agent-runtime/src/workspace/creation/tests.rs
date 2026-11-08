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
    fn grant(&self, j: &Journal) -> CreateFileGrant {
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
    let identity = ExecutionIdentity {
        connection_epoch: 1,
        fencing: 1,
        attempt: 1,
    };
    j.commit_response(
        "alice",
        "run",
        "invocation",
        ResponseAuthority {
            current: identity,
            incoming: identity,
            complete: true,
        },
        &"a".repeat(64),
    )
    .expect("explicit complete model response fixture");
}
fn operation<'a>(id: &'a str, digest: &'a str, grant: &'a CreateFileGrant) -> Operation<'a> {
    Operation {
        id,
        owner: "alice",
        run: "run",
        invocation: "invocation",
        argument_digest: digest,
        tool_version: "write_file.v1",
        environment: grant.environment(),
        policy_version: "policy",
        requires_approval: true,
    }
}
fn approve(j: &mut Journal, op: &Operation<'_>) {
    j.prepare(op).expect("durable intent");
    j.approve(op, "alice", i64::MAX)
        .expect("explicit owner approval");
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
    let grant = f.grant(&j);
    let input = args("new.txt", "actual content");
    let digest = input.digest().expect("digest");
    let op = operation("create-1", &digest, &grant);
    j.register_run("alice", "run", "policy").expect("run");
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Journal(JournalError::ResponseRequired))
    ));
    response(&j);
    assert!(matches!(
        j.create_file(&grant, &op, input.clone()).await,
        Err(CreateFileError::Journal(JournalError::ApprovalRequired))
    ));
    assert!(!f.root.path().join("new.txt").exists());
    approve(&mut j, &op);
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
    let grant = f.grant(&j);
    fs::create_dir(f.root.path().join("中文")).expect("parent");
    fs::write(f.root.path().join("中文/你好.txt"), "original").expect("existing file");
    let input = args("中文/你好.txt", "replacement");
    let digest = input.digest().expect("digest");
    let op = operation("conflict", &digest, &grant);
    approve(&mut j, &op);
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
    approve(&mut j, &op);
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
    let grant = f.grant(&j);
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
        approve(&mut j, &op);
        assert!(j.create_file(&grant, &op, input).await.is_err());
        assert!(!outside.path().join("escape.txt").exists());
    }
    let op = operation("revoked", &digest, &grant);
    approve(&mut j, &op);
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
    let grant = f.grant(&j);
    let input = args("created.txt", "first publication");
    let digest = input.digest().expect("digest");
    let op = operation("faulted", &digest, &grant);
    approve(&mut j, &op);
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
    let grant = f.grant(&j);
    let op = operation("faulted", &digest, &grant);
    fs::write(f.root.path().join("created.txt"), "user edit after crash").expect("user change");
    assert!(matches!(
        j.create_file(&grant, &op, input).await,
        Err(CreateFileError::Journal(JournalError::AlreadyDispatched))
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
    let ga = f.grant(&a);
    let gb = f.grant(&b);
    let aa = args("race.txt", &"α".repeat(20_000));
    let ab = args("race.txt", &"β".repeat(20_000));
    let da = aa.digest().expect("digest");
    let db = ab.digest().expect("digest");
    let oa = operation("race-a", &da, &ga);
    let ob = operation("race-b", &db, &gb);
    approve(&mut a, &oa);
    approve(&mut b, &ob);
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
    let grant = f.grant(&j);
    let input = args("new.txt", "body");
    let digest = input.digest().expect("digest");
    let mut op = operation("expired", &digest, &grant);
    j.prepare(&op).expect("intent");
    j.approve(&op, "alice", 0).expect("expired fixture");
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
