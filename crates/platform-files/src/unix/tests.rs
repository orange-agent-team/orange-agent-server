use super::*;
use std::{fs, os::unix::fs::PermissionsExt};
#[test]
fn distinct_file_metadata_copy_preserves_mode_and_xattr_without_copying_content() {
    let root = tempfile::tempdir().expect("fixture");
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"source bytes").expect("source");
    fs::write(&target, b"new bytes").expect("target");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("mode");
    let source_file = fs::File::open(&source).expect("source fd");
    let target_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .expect("target fd");
    #[cfg(target_vendor = "apple")]
    let attr = "com.orangeagent.fixture";
    #[cfg(not(target_vendor = "apple"))]
    let attr = "user.orangeagent.fixture";
    rustix::fs::fsetxattr(
        &source_file,
        attr,
        b"owned metadata",
        rustix::fs::XattrFlags::empty(),
    )
    .expect("attribute");
    let before = security_metadata(&source_file).expect("source metadata");
    let copied = copy_security_metadata(&source_file, &target_file).expect("copy metadata");
    assert_eq!(copied, before);
    assert_eq!(
        security_metadata(&target_file).expect("target metadata"),
        before
    );
    assert_eq!(
        fs::read(&source).expect("source unchanged"),
        b"source bytes"
    );
    assert_eq!(
        fs::read(&target).expect("target content untouched"),
        b"new bytes"
    );
    assert!(copy_security_metadata(&source_file, &source_file).is_err());
}
#[cfg(target_vendor = "apple")]
#[test]
fn extended_acl_is_preserved_on_an_owned_fixture() {
    let root = tempfile::tempdir().expect("fixture");
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"source").expect("source");
    fs::write(&target, b"target").expect("target");
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow read"])
        .arg(&source)
        .status()
        .expect("owned fixture ACL");
    assert!(status.success());
    let a = fs::File::open(&source).expect("source");
    let b = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .expect("target");
    let expected = security_metadata(&a).expect("ACL metadata");
    assert!(!expected.acl.is_empty());
    copy_security_metadata(&a, &b).expect("ACL copy");
    assert_eq!(security_metadata(&b).expect("ACL preserved"), expected);
}

#[test]
fn hardlinked_destination_is_refused_before_mutating_metadata() {
    let root = tempfile::tempdir().expect("fixture");
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"source").expect("source");
    fs::write(&target, b"target").expect("target");
    fs::hard_link(&target, root.path().join("alias")).expect("alias");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("mode");
    let a = fs::File::open(&source).expect("source fd");
    let b = fs::File::options()
        .read(true)
        .write(true)
        .open(&target)
        .expect("target fd");
    let before = security_metadata(&b).expect("before");
    assert!(copy_security_metadata(&a, &b).is_err());
    assert_eq!(security_metadata(&b).expect("after"), before);
    assert_eq!(fs::read(&target).expect("contents"), b"target");
}

#[cfg(target_vendor = "apple")]
#[test]
fn absent_acl_on_open_unlinked_file_is_not_a_missing_path_error() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("source");
    fs::write(&path, b"owned").expect("source");
    let file = fs::File::open(&path).expect("fd");
    let before = security_metadata(&file).expect("no ACL");
    assert!(before.acl.is_empty());
    fs::remove_file(path).expect("unlink owned fixture");
    assert_eq!(security_metadata(&file).expect("live descriptor"), before);
}

#[cfg(target_vendor = "apple")]
#[test]
fn oversized_metadata_refuses_copy_before_changing_target() {
    let root = tempfile::tempdir().expect("fixture");
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"source").expect("source");
    fs::write(&target, b"target").expect("target");
    let a = fs::File::open(source).expect("source fd");
    let b = fs::File::options()
        .read(true)
        .write(true)
        .open(&target)
        .expect("target fd");
    let before = security_metadata(&b).expect("before");
    rustix::fs::fsetxattr(
        &a,
        "com.orangeagent.fixture",
        &vec![1; MAX_VALUE + 1],
        rustix::fs::XattrFlags::empty(),
    )
    .expect("oversized owned attribute");
    assert!(security_metadata(&a).is_err());
    assert!(copy_security_metadata(&a, &b).is_err());
    assert_eq!(security_metadata(&b).expect("after"), before);
    assert_eq!(fs::read(target).expect("contents"), b"target");
}

#[cfg(target_vendor = "apple")]
#[test]
fn snapshot_digest_changes_for_acl_and_xattr_changes() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("source");
    fs::write(&path, b"same bytes").expect("source");
    let file = fs::File::open(&path).expect("fd");
    let original = security_metadata(&file).expect("original").digest();
    rustix::fs::fsetxattr(
        &file,
        "com.orangeagent.fixture",
        b"one",
        rustix::fs::XattrFlags::empty(),
    )
    .expect("attribute");
    let with_xattr = security_metadata(&file).expect("with attribute").digest();
    assert_ne!(original, with_xattr);
    assert!(
        std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&path)
            .status()
            .expect("ACL")
            .success()
    );
    assert_ne!(
        with_xattr,
        security_metadata(&file).expect("with ACL").digest()
    );
    assert_eq!(fs::read(path).expect("content unchanged"), b"same bytes");
}
