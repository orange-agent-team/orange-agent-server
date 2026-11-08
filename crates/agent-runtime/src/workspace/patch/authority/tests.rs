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
