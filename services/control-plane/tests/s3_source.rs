//! Real signed requests to a disposable SeaweedFS 4.52 service. No platform object writer involved.
use orange_control_plane::s3_objects::{Error, Registration};
use sha2::{Digest, Sha256};
use std::{env, time::Duration};
#[tokio::test]
async fn read_registered_prefix_and_fence_replaced_objects() {
    let port = env::var("ORANGE_CN06_PORT")
        .expect("fixture port")
        .parse()
        .expect("port");
    let cfg = Registration {
        address: std::net::Ipv4Addr::LOCALHOST.into(),
        port,
        region: "us-east-1".into(),
        bucket: "registered-data".into(),
        prefix: "team/approved".into(),
    };
    let key = env::var("ORANGE_CN06_KEY").expect("test reader");
    let secret = env::var("ORANGE_CN06_SECRET").expect("test reader");
    let src = cfg
        .open_development(&key, &secret)
        .expect("registered source");
    let first = src.list(None, 1).await.expect("first page");
    assert_eq!(first.keys, ["a.txt"]);
    assert_eq!(first.next_after.as_deref(), Some("a.txt"));
    let second = src
        .list(first.next_after.as_deref(), 5)
        .await
        .expect("signed list cursor");
    assert_eq!(
        second.keys,
        [
            "b.txt",
            "empty.txt",
            "large.bin",
            "nested/橙子.txt",
            "percent 100%.txt"
        ]
    );
    assert!(second.next_after.is_none());
    for path in [
        "../private.txt",
        "/team/private",
        "a//file",
        "%2e%2e/private",
        "nested\\bad",
    ] {
        assert!(matches!(src.head(path).await, Err(Error::Invalid)));
    }
    assert!(matches!(src.list(None, 101).await, Err(Error::Invalid)));
    let percent = src
        .head("percent 100%.txt")
        .await
        .expect("literal percent key");
    assert_eq!(
        src.read(&percent).await.expect("literal key bytes").bytes,
        b"literal percent"
    );
    let head = src.head("a.txt").await.expect("signed HEAD");
    let data = src.read(&head).await.expect("conditional source read");
    assert_eq!(data.bytes, b"confirmed source data");
    assert_eq!(data.sha256, hex::encode(Sha256::digest(&data.bytes)));
    let empty = src.head("empty.txt").await.expect("empty metadata");
    assert!(
        src.read(&empty)
            .await
            .expect("empty content")
            .bytes
            .is_empty()
    );
    let large = src
        .head("large.bin")
        .await
        .expect("oversized object metadata");
    assert!(matches!(src.read(&large).await, Err(Error::Limit)));
    let bad = cfg
        .open_development(&key, "incorrect")
        .expect("incorrect credential fixture");
    assert!(bad.head("a.txt").await.is_err());
    let dir = std::path::PathBuf::from(env::var("ORANGE_CN06_FENCE").expect("fixture gate"));
    std::fs::write(dir.join("head-recorded"), b"1").expect("release object writer");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dir.join("object-replaced").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real object mutation");
    assert!(matches!(src.read(&head).await, Err(Error::Changed)));
    let current = src.head("a.txt").await.expect("current head");
    assert_ne!(head.etag, current.etag);
    assert_eq!(
        src.read(&current).await.expect("new bytes").bytes,
        b"updated source data"
    );
}
