use super::*;
use std::{fs, os::unix::fs::PermissionsExt};
#[cfg(target_vendor = "apple")]
#[test]
fn publication_timestamp_policy_preserves_all_authority_and_identity_fields() {
    let root = tempfile::tempdir().expect("owned fixture");
    let path = root.path().join("source");
    fs::write(&path, b"original").expect("write");
    let file = fs::File::open(path).expect("open");
    let mut source = security_metadata(&file).expect("metadata");
    source
        .attributes
        .retain(|(n, _)| n != b"com.apple.quarantine");
    source.attributes.push((
        b"com.apple.quarantine".to_vec(),
        b"0086;65000000;Orange Agent;".to_vec(),
    ));
    source.attributes.sort_by(|a, b| a.0.cmp(&b.0));
    let index = source
        .attributes
        .iter()
        .position(|(n, _)| n == b"com.apple.quarantine")
        .expect("label");
    let mut target = source.clone();
    target.attributes[index].1 = b"0086;65000001;Orange Agent;".to_vec();
    assert!(preserves_publication(&source, &target));
    assert_ne!(
        source.digest(),
        target.digest(),
        "actual target digest must retain changed timestamp"
    );
    for forbidden in [
        "0082;65000001;Orange Agent;", // dropped flag
        "00c6;65000001;Orange Agent;", // added flag: do not infer its meaning
        "0086;65000001;Other Agent;",
        "0086;65000001;Orange Agent;00000000-0000-0000-0000-000000000000",
        "0086;64000000;Orange Agent;", // older
        "0086;ffffffff;Orange Agent;", // future
        "0086;65000001;Orange Agent",
        "0086;65000001;Orange Agent;;",
        "0086;invalid!;Orange Agent;",
        "0086;65000001;;",
        "0086;65000001;Orange Agent;unknown",
    ] {
        let mut changed = target.clone();
        changed.attributes[index].1 = forbidden.as_bytes().to_vec();
        assert!(
            !preserves_publication(&source, &changed),
            "accepted changed security label: {forbidden}"
        );
    }
    for dimension in 0..8 {
        let mut changed = target.clone();
        match dimension {
            0 => changed.uid += 1,
            1 => changed.gid += 1,
            2 => changed.mode ^= 0o111,
            3 => changed.flags ^= 1,
            4 => changed.acl.push(1),
            5 => {
                changed.attributes.remove(index);
            }
            6 => changed.attributes.push((b"new".to_vec(), vec![1])),
            _ => changed.attributes[index].0 = b"unknown".to_vec(),
        }
        assert!(!preserves_publication(&source, &changed));
    }
    let mut no_label = source.clone();
    no_label.attributes.remove(index);
    assert!(
        !preserves_publication(&no_label, &target),
        "label introduction is not certified"
    );
    assert!(
        !preserves_publication(&target, &no_label),
        "label removal is forbidden"
    );
}
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
