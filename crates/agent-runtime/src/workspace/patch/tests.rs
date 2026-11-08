use super::*;
fn args(source: &[u8], patch: &str) -> PatchArgs {
    PatchArgs {
        path: "src/例子.txt".into(),
        expected_sha256: hex::encode(Sha256::digest(source)),
        patch: patch.into(),
    }
}
#[test]
fn exact_multiple_hunks_preserve_untouched_unicode_and_report_complete_preview() {
    let source = "一\nsecond\nkeep\nfourth\n五\n".as_bytes();
    let patch = "@@ -1,2 +1,3 @@\n 一\n-second\n+two\n+extra\n@@ -4,2 +5,2 @@ section\n-fourth\n+four\n 五\n";
    let input = args(source, patch);
    let prepared = prepare(&input, source).expect("exact patch");
    assert_eq!(
        prepared.content(),
        "一\ntwo\nextra\nkeep\nfour\n五\n".as_bytes()
    );
    let preview = prepared.preview();
    assert_eq!(
        (preview.hunks, preview.added_lines, preview.removed_lines),
        (2, 3, 2)
    );
    assert_eq!(preview.argument_digest, input.digest().expect("digest"));
    assert!(preview.diff.ends_with(patch));
    assert_eq!(
        preview.result_sha256,
        hex::encode(Sha256::digest(prepared.content()))
    );
}
#[test]
fn empty_files_insertions_and_empty_replacement_do_not_imply_file_deletion() {
    let empty = b"";
    assert_eq!(
        prepare(&args(empty, "@@ -0,0 +1,1 @@\n+first\n"), empty)
            .expect("empty insert")
            .content(),
        b"first\n"
    );
    let source = b"a\nb\n";
    assert_eq!(
        prepare(&args(source, "@@ -1,0 +2,1 @@\n+middle\n"), source)
            .expect("insertion")
            .content(),
        b"a\nmiddle\nb\n"
    );
    assert_eq!(
        prepare(&args(source, "@@ -1,2 +0,0 @@\n-a\n-b\n"), source)
            .expect("empty content")
            .content(),
        b""
    );
}
#[test]
fn newline_markers_and_crlf_are_exact_and_never_normalized() {
    let source = b"old";
    let patch =
        "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
    assert_eq!(
        prepare(&args(source, patch), source)
            .expect("no newline")
            .content(),
        b"new"
    );
    let source = b"a\r\nb\r\n";
    let patch = "@@ -1,2 +1,2 @@\n a\r\n-b\r\n+B\r\n";
    assert_eq!(
        prepare(&args(source, patch), source)
            .expect("CRLF preserved")
            .content(),
        b"a\r\nB\r\n"
    );
    assert!(matches!(
        prepare(&args(source, "@@ -1 +1 @@\n-a\n+A\n"), source),
        Err(PatchError::ContextChanged)
    ));
}
#[test]
fn wrong_digest_or_context_has_no_fuzzy_fallback() {
    let source = b"same\nvalue\n";
    let mut input = args(source, "@@ -2 +2 @@\n-value\n+new\n");
    input.expected_sha256 = "0".repeat(64);
    assert!(matches!(
        prepare(&input, source),
        Err(PatchError::VersionChanged)
    ));
    assert!(matches!(
        prepare(&args(source, "@@ -1 +1 @@\n-value\n+new\n"), source),
        Err(PatchError::ContextChanged)
    ));
    assert!(matches!(
        prepare(&args(source, "@@ -1 +1 @@\n same\n"), source),
        Err(PatchError::NoChange)
    ));
}
#[test]
fn malformed_overlapping_and_multi_file_patches_are_rejected() {
    let source = b"a\nb\nc\n";
    for patch in [
        "--- a/a\n+++ b/b\n@@ -1 +1 @@\n-a\n+A\n",
        "*** Delete File: a\n",
        "@@ -0,1 +1,1 @@\n-a\n+A\n",
        "@@ -1,2 +1,1 @@\n-a\n+A\n",
        "@@ -1 +2 @@\n-a\n+A\n",
        "@@ -1 +1 @@\n-a\n+A\n@@ -1 +1 @@\n-a\n+B\n",
        "@@ -1 +1 @@\n-a\n+A",
        "@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+A\n",
        "@@ -1 +1 @@\n-a\n+A\n\\ No newline at end of file\n",
    ] {
        assert!(prepare(&args(source, patch), source).is_err(), "{patch:?}");
    }
}
#[test]
fn source_patch_and_expansion_limits_are_enforced_before_execution() {
    assert!(matches!(
        prepare(&args(b"\xff", "@@ -1 +1 @@\n-x\n+y\n"), b"\xff"),
        Err(PatchError::NotText)
    ));
    assert!(matches!(
        prepare(&args(b"a\0b", "@@ -1 +1 @@\n-x\n+y\n"), b"a\0b"),
        Err(PatchError::NotText)
    ));
    let source = vec![b'a'; MAX_TEXT + 1];
    assert!(matches!(
        prepare(&args(&source, "@@ -1 +1 @@\n-a\n+b\n"), &source),
        Err(PatchError::Limit)
    ));
    let source = "a\n".repeat(MAX_TEXT / 2);
    let patch = "@@ -0,0 +1,1 @@\n+extra\n";
    assert!(matches!(
        prepare(&args(source.as_bytes(), patch), source.as_bytes()),
        Err(PatchError::Limit)
    ));
    let source = b"a\n";
    let patch = format!("@@ -1 +1 @@\n-a\n+{}\n", "b".repeat(MAX_PATCH));
    assert!(matches!(
        prepare(&args(source, &patch), source),
        Err(PatchError::Limit)
    ));
}
#[test]
fn protected_paths_and_unknown_authority_fields_are_not_patch_arguments() {
    let source = b"a\n";
    let mut input = args(source, "@@ -1 +1 @@\n-a\n+b\n");
    for path in [
        "../outside",
        "/absolute",
        ".ssh/config",
        ".Kube/config",
        "client.Key",
    ] {
        input.path = path.into();
        assert!(input.validate().is_err());
    }
    assert!(serde_json::from_value::<PatchArgs>(serde_json::json!({"path":"a","expected_sha256":"a".repeat(64),"patch":"@@ -1 +1 @@\n-a\n+b\n","overwrite_anyway":true})).is_err());
}

#[test]
fn hunk_count_and_eof_markers_cannot_encode_ambiguous_line_streams() {
    let source = "a\n".repeat(129);
    let mut patch = String::new();
    for n in 1..=129 {
        patch.push_str(&format!("@@ -{n} +{n} @@\n-a\n+b\n"));
    }
    assert!(matches!(
        prepare(&args(source.as_bytes(), &patch), source.as_bytes()),
        Err(PatchError::Limit)
    ));
    for patch in [
        "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+\n\\ No newline at end of file\n",
        "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n\\ No newline at end of file\n+new\n",
        "@@ -999999999999 +1 @@\n-old\n+new\n",
    ] {
        assert!(prepare(&args(b"old", patch), b"old").is_err());
    }
}
