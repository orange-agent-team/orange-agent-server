use super::*;
use crate::storage::private_temp;
use std::{fs, sync::Arc};
use tokio::sync::Notify;
fn input(bytes: &[u8]) -> PatchArgs {
    PatchArgs {
        path: "file.txt".into(),
        expected_sha256: hex::encode(Sha256::digest(bytes)),
        patch: "@@ -1 +1 @@\n-old\n+new\n".into(),
    }
}
struct Fixture {
    private: tempfile::TempDir,
    root: tempfile::TempDir,
    read: WorkspaceGrant,
    edit: EditFileGrant,
}
#[cfg(target_vendor = "apple")]
#[tokio::test]
async fn security_attributes_are_pinned_and_cannot_reuse_old_review() {
    let f = Fixture::new();
    let path = f.root.path().join("file.txt");
    let original = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("original");
    assert_eq!(original.identity.security_digest.len(), 64);
    let file = fs::File::open(&path).expect("owned fd");
    rustix::fs::fsetxattr(
        &file,
        "com.orangeagent.fixture",
        b"changed",
        rustix::fs::XattrFlags::empty(),
    )
    .expect("attribute");
    assert!(f.edit.revalidate(&f.read, &original).await.is_err());
    let changed = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("new attribute review");
    assert_ne!(
        original.identity.security_digest,
        changed.identity.security_digest
    );
    assert!(
        std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&path)
            .status()
            .expect("ACL")
            .success()
    );
    assert!(f.edit.revalidate(&f.read, &changed).await.is_err());
    let with_acl = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("ACL review");
    assert_ne!(
        changed.identity.security_digest,
        with_acl.identity.security_digest
    );
    assert_eq!(fs::read(path).expect("unchanged content"), b"old\n");
}
impl Fixture {
    fn new() -> Self {
        let private = private_temp();
        let root = tempfile::tempdir().expect("workspace");
        fs::write(root.path().join("file.txt"), b"old\n").expect("source");
        let read = WorkspaceGrant::open(
            "alice",
            "read-consent",
            "policy",
            root.path(),
            &[private.path().into()],
        )
        .expect("read consent");
        let edit = EditFileGrant::open(
            "alice",
            "separate-edit-consent",
            "policy",
            root.path(),
            &[private.path().into()],
        )
        .expect("edit consent");
        Self {
            private,
            root,
            read,
            edit,
        }
    }
}
#[tokio::test]
async fn actual_preview_requires_both_authorities_and_changes_no_files() {
    let f = Fixture::new();
    let args = input(b"old\n");
    let preview = f
        .edit
        .preview(&f.read, args)
        .await
        .expect("actual authorized preview");
    assert_eq!(preview.content(), b"new\n");
    assert_eq!(
        preview.preview().original_sha256,
        hex::encode(Sha256::digest(b"old\n"))
    );
    assert_eq!(preview.source_binding_digest().len(), 64);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
    let current = f
        .edit
        .revalidate(&f.read, &preview)
        .await
        .expect("same live object");
    assert_eq!(
        current.source_binding_digest(),
        preview.source_binding_digest()
    );
    let other = tempfile::tempdir().expect("other root");
    let foreign = WorkspaceGrant::open(
        "alice",
        "other",
        "policy",
        other.path(),
        &[f.private.path().into()],
    )
    .expect("other consent");
    assert!(f.edit.preview(&foreign, input(b"old\n")).await.is_err());
    let bob = WorkspaceGrant::open(
        "bob",
        "read",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("other owner");
    assert!(f.edit.preview(&bob, input(b"old\n")).await.is_err());
}
#[tokio::test]
async fn changed_bytes_same_bytes_replacement_and_mode_change_invalidate_preview() {
    let f = Fixture::new();
    let preview = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("preview");
    fs::write(f.root.path().join("file.txt"), b"user change\n").expect("external edit");
    assert!(f.edit.revalidate(&f.read, &preview).await.is_err());
    fs::remove_file(f.root.path().join("file.txt")).expect("replace owned fixture");
    fs::write(f.root.path().join("file.txt"), b"old\n").expect("same bytes new object");
    assert!(f.edit.revalidate(&f.read, &preview).await.is_err());
    let preview = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("new review");
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(f.root.path().join("file.txt"))
        .expect("mode")
        .permissions()
        .mode();
    fs::set_permissions(
        f.root.path().join("file.txt"),
        fs::Permissions::from_mode(mode ^ 0o100),
    )
    .expect("external mode change");
    assert!(f.edit.revalidate(&f.read, &preview).await.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("preserved"),
        b"old\n"
    );
}
#[tokio::test]
async fn links_private_paths_and_new_authority_cannot_reuse_old_preview() {
    let f = Fixture::new();
    let external = tempfile::tempdir().expect("external");
    fs::write(external.path().join("secret"), b"old\n").expect("owned external fixture");
    fs::remove_file(f.root.path().join("file.txt")).expect("remove owned fixture");
    std::os::unix::fs::symlink(
        external.path().join("secret"),
        f.root.path().join("file.txt"),
    )
    .expect("link");
    assert!(f.edit.preview(&f.read, input(b"old\n")).await.is_err());
    fs::remove_file(f.root.path().join("file.txt")).expect("remove link");
    fs::hard_link(
        external.path().join("secret"),
        f.root.path().join("file.txt"),
    )
    .expect("hardlink");
    assert!(f.edit.preview(&f.read, input(b"old\n")).await.is_err());
    assert_eq!(
        fs::read(external.path().join("secret")).expect("untouched"),
        b"old\n"
    );
    assert!(
        EditFileGrant::open(
            "alice",
            "forbidden",
            "policy",
            f.private.path(),
            &[f.private.path().into()]
        )
        .is_err()
    );
    fs::remove_file(f.root.path().join("file.txt")).expect("unlink owned name");
    fs::write(f.root.path().join("file.txt"), b"old\n").expect("new source");
    let preview = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("preview");
    let replacement = EditFileGrant::open(
        "alice",
        "new-edit-consent",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("new explicit consent");
    assert!(replacement.revalidate(&f.read, &preview).await.is_err());
    let new_read = WorkspaceGrant::open(
        "alice",
        "new-read-consent",
        "policy",
        f.root.path(),
        &[f.private.path().into()],
    )
    .expect("new read");
    assert!(f.edit.revalidate(&new_read, &preview).await.is_err());
}
#[tokio::test]
async fn preview_holds_permission_leases_until_persisted_or_discarded() {
    let f = Fixture::new();
    let preview = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("preview");
    assert!(preview.is_authorized());
    f.edit.request_revoke();
    assert!(!preview.is_authorized());
    assert!(f.edit.revalidate(&f.read, &preview).await.is_err());
    let drained = Arc::new(Notify::new());
    let signal = drained.clone();
    let edit = f.edit.clone();
    let waiting = tokio::spawn(async move {
        edit.wait_drained().await;
        signal.notify_one();
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), drained.notified())
            .await
            .is_err()
    );
    drop(preview);
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
        .await
        .expect("lease released")
        .expect("wait task");
    assert!(
        !f.read.is_revoked(),
        "edit revocation does not expand or revoke independent read rights"
    );
    assert!(f.edit.preview(&f.read, input(b"old\n")).await.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no effects"),
        b"old\n"
    );
}

#[tokio::test]
async fn read_revocation_invalidates_the_edit_preview_without_file_effects() {
    let f = Fixture::new();
    let preview = f
        .edit
        .preview(&f.read, input(b"old\n"))
        .await
        .expect("preview");
    f.read.request_revoke();
    assert!(!preview.is_authorized());
    assert!(f.edit.revalidate(&f.read, &preview).await.is_err());
    assert!(f.edit.preview(&f.read, input(b"old\n")).await.is_err());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("original"),
        b"old\n"
    );
    drop(preview);
    f.read.wait_drained().await;
}

#[tokio::test]
async fn durable_edit_consent_is_separate_owned_idempotent_and_epoch_bound() {
    let f = Fixture::new();
    let path = f.private.path().join("journal.sqlite");
    let mut j = crate::Journal::open(&path).expect("journal");
    let readonly = j
        .authorize_workspace("alice", "read-store", f.root.path(), "policy")
        .expect("durable readonly");
    let create = j
        .authorize_file_creation("alice", "create-store", "policy", f.root.path())
        .expect("create-only consent");
    assert!(j.editing_consents("alice").expect("inventory").is_empty());
    assert!(
        j.preview_file_edit(&f.edit, &f.read, input(b"old\n"))
            .await
            .is_err(),
        "transient review grant is not durable edit permission"
    );
    let edit = j
        .authorize_file_edit("alice", "explicit-edit", "policy", f.root.path())
        .expect("separate edit consent");
    assert_ne!(edit.environment(), create.environment());
    assert_ne!(edit.environment(), f.read.environment());
    let id = edit.consent_id().to_owned();
    assert_eq!(
        j.authorize_file_edit("alice", "explicit-edit", "policy", f.root.path())
            .expect("retry")
            .consent_id(),
        id
    );
    assert!(j.editing_consent("bob", &id).is_err());
    let preview = j
        .preview_file_edit(&edit, &f.read, input(b"old\n"))
        .await
        .expect("actual preview");
    assert_eq!(preview.content(), b"new\n");
    drop(preview);
    drop(j);
    let mut j = crate::Journal::open(&path).expect("reopen");
    assert!(
        j.preview_file_edit(&edit, &f.read, input(b"old\n"))
            .await
            .is_err(),
        "old Host handle never restores itself"
    );
    let restored = j
        .restore_file_edit("alice", &id)
        .expect("explicit fresh access");
    let preview = j
        .preview_file_edit(&restored, &f.read, input(b"old\n"))
        .await
        .expect("fresh review");
    drop(preview);
    assert!(
        j.workspace("alice", &readonly.workspace_id)
            .expect("readonly retained")
            .active
    );
    assert!(
        j.creation_consent("alice", create.consent_id())
            .expect("create retained")
            .active
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
}
#[tokio::test]
async fn durable_edit_revocation_rolls_back_without_reviving_live_handles() {
    let f = Fixture::new();
    let mut j = crate::Journal::open(&f.private.path().join("journal.sqlite")).expect("journal");
    let edit = j
        .authorize_file_edit("alice", "edit", "policy", f.root.path())
        .expect("consent");
    let id = edit.consent_id().to_owned();
    let version = edit.consent_version().expect("persistent");
    j.connection.execute_batch("CREATE TRIGGER fail_edit_revoke BEFORE UPDATE ON editing_consents BEGIN SELECT RAISE(ABORT,'injected failure');END;").expect("fault");
    assert!(j.revoke_file_edit("alice", &id, &version).is_err());
    assert!(
        j.editing_consent("alice", &id)
            .expect("durable state")
            .active
    );
    assert!(
        j.preview_file_edit(&edit, &f.read, input(b"old\n"))
            .await
            .is_err()
    );
    assert!(j.restore_file_edit("alice", &id).is_err());
    j.connection
        .execute_batch("DROP TRIGGER fail_edit_revoke;")
        .expect("repair");
    assert!(
        !j.revoke_file_edit("alice", &id, &version)
            .expect("durable removal")
            .active
    );
    assert!(
        j.authorize_file_edit("alice", "edit", "policy", f.root.path())
            .is_err(),
        "old request cannot reactivate permission"
    );
    assert!(!f.read.is_revoked());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("unchanged"),
        b"old\n"
    );
}
#[tokio::test]
async fn edit_private_namespace_and_changed_resource_cannot_inherit_permission() {
    let f = Fixture::new();
    let mut j = crate::Journal::open(&f.private.path().join("journal.sqlite")).expect("journal");
    let edit = j
        .authorize_file_edit("alice", "edit", "policy", f.root.path())
        .expect("consent");
    let private = f.root.path().join("future-private");
    fs::create_dir(&private).expect("fixture");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("mode");
    assert!(
        j.reserve_private_directory(&private).is_err(),
        "private namespace must not overlap active editors"
    );
    let retained = tempfile::tempdir().expect("owned retention area");
    let original = retained.path().join("original");
    fs::rename(f.root.path(), &original).expect("move owned directory");
    fs::create_dir(f.root.path()).expect("replacement");
    assert!(j.restore_file_edit("alice", edit.consent_id()).is_err());
    assert!(
        !j.editing_consent("alice", edit.consent_id())
            .expect("identity revocation")
            .active
    );
    fs::remove_dir_all(&original).expect("cleanup owned moved fixture");
}

#[tokio::test]
async fn schema_seven_migration_preserves_existing_rights_without_inventing_edit_consent() {
    let f = Fixture::new();
    let path = f.private.path().join("journal.sqlite");
    let mut j = crate::Journal::open(&path).expect("journal");
    let read = j
        .authorize_workspace("alice", "read", f.root.path(), "policy")
        .expect("read consent");
    let create = j
        .authorize_file_creation("alice", "create", "policy", f.root.path())
        .expect("create consent");
    let create_id = create.consent_id().to_owned();
    j.connection.execute_batch("DROP TABLE runtime_memory_sources; DROP TABLE runtime_memory_operations; DROP TABLE runtime_memory_capabilities; DROP TABLE memory_source_blocks; DROP TABLE memory_audit; DROP TABLE memory_drafts; DROP TABLE memory_entries; DROP TABLE memory_members; DROP TABLE memory_scopes; DROP TABLE export_restorations; DROP TABLE export_retirement_capsules; DROP TABLE export_retirements; DROP TABLE export_writer_bindings; DROP TABLE patch_exports; DROP TABLE patch_capture_receipts; DROP TABLE patch_capture_witnesses; DROP TABLE runtime_edit_capabilities; DROP TABLE patch_version_edges; DROP TABLE patch_retention_archives; DROP TABLE patch_retention_reviews; DROP TABLE patch_recovery_resolutions; DROP TABLE patch_recovery_results; DROP TABLE patch_recovery_approvals; DROP TABLE patch_recoveries; DROP TABLE editing_publication_steps; DROP TABLE editing_publications; DROP TABLE editing_approvals; DROP TABLE editing_previews; DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; PRAGMA user_version=7;").expect("actual prior schema fixture");
    drop(j);
    let j = crate::Journal::open(&path).expect("migrate");
    assert!(j.editing_consents("alice").expect("inventory").is_empty());
    assert!(
        j.workspace("alice", &read.workspace_id)
            .expect("read retained")
            .active
    );
    assert!(
        j.creation_consent("alice", &create_id)
            .expect("create retained")
            .active
    );
    let version: i64 = j
        .connection
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, 23);
    assert!(
        j.preview_file_edit(&f.edit, &f.read, input(b"old\n"))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn edit_identity_history_and_request_keys_are_immutable() {
    let f = Fixture::new();
    let mut j = crate::Journal::open(&f.private.path().join("journal.sqlite")).expect("journal");
    let edit = j
        .authorize_file_edit("alice", "edit", "policy", f.root.path())
        .expect("consent");
    assert!(
        j.connection
            .execute("UPDATE editing_consents SET environment='changed'", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM editing_consents", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("UPDATE editing_request_keys SET request_id='changed'", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM editing_consent_audit", [])
            .is_err()
    );
    let revoker = j
        .editing_revoker("alice", edit.consent_id())
        .expect("signal");
    revoker.request_revoke();
    assert!(j.restore_file_edit("alice", edit.consent_id()).is_err());
    assert!(
        j.revoke_file_edit("alice", edit.consent_id(), "99")
            .is_err()
    );
    assert!(
        j.editing_consent("alice", edit.consent_id())
            .expect("not falsely acknowledged")
            .active
    );
    j.revoke_file_edit(
        "alice",
        edit.consent_id(),
        &edit.consent_version().expect("version"),
    )
    .expect("correct durable revoke");
    assert!(
        j.connection
            .execute("UPDATE editing_consents SET active=1,version=version+1", [])
            .is_err()
    );
}
#[tokio::test]
async fn unresolved_edit_effect_blocks_fresh_edit_create_and_read_permission_for_resource() {
    let f = Fixture::new();
    let mut j = crate::Journal::open(&f.private.path().join("journal.sqlite")).expect("journal");
    let edit = j
        .authorize_file_edit("alice", "edit", "policy", f.root.path())
        .expect("consent");
    j.register_run("alice", "uncertain", "policy")
        .expect("owned Run");
    j.connection.execute("INSERT INTO responses(invocation,run_id,attempt,digest) VALUES('fixture-inv','uncertain',1,?)",["a".repeat(64)]).expect("explicit uncertain-effect fixture");
    j.connection.execute("INSERT INTO operations(operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state) VALUES('fixture-op','alice','uncertain','fixture-inv',?,'apply_patch.v1',?,'policy',1,'unknown')",rusqlite::params!["b".repeat(64),edit.environment()]).expect("unknown fixture, no actual write");
    assert!(j.restore_file_edit("alice", edit.consent_id()).is_err());
    assert!(
        j.authorize_file_edit("alice", "fresh-edit", "policy", f.root.path())
            .is_err()
    );
    assert!(
        j.authorize_file_creation("alice", "fresh-create", "policy", f.root.path())
            .is_err()
    );
    assert!(
        j.authorize_workspace("alice", "fresh-read", f.root.path(), "policy")
            .is_err()
    );
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no test write"),
        b"old\n"
    );
}
