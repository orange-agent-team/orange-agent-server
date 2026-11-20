//! Public, unauthenticated provider qualification. Real private OAuth grants are a separate gate.
use orange_control_plane::repo_connector::{Error, Kind, Provider, Registration, Repository};
use serde_json::json;
use std::collections::BTreeSet;
async fn source(provider: Provider, id: u64, path: &str, revision: &str, file: &str) {
    let allowed = BTreeSet::from([id]);
    let source = Registration {
        provider,
        repositories: vec![Repository {
            id,
            path: path.into(),
            file_prefixes: vec![String::new()],
            issues: true,
            changes: true,
        }],
    }
    .open(None)
    .expect("registered HTTPS source");
    assert!(matches!(
        source.read_file(&BTreeSet::new(), id, revision, file).await,
        Err(Error::Denied)
    ));
    let repos = source
        .list_repositories(&allowed, 1, 2)
        .await
        .expect("real registered repository");
    assert_eq!(repos.data.len(), 1);
    assert_eq!(repos.data[0]["id"], id.to_string());
    let blob = source
        .read_file(&allowed, id, revision, file)
        .await
        .expect("provider commit tree and Git blob");
    assert_eq!(blob.path, file);
    assert!(!blob.bytes.is_empty());
    assert!(blob.bytes.len() <= 20 * 1024 * 1024);
    let mut ids = Vec::new();
    for kind in [Kind::Issue, Kind::Change] {
        let mut page = 1;
        let sample = loop {
            let list = source
                .list_items(&allowed, id, kind, page, 15)
                .await
                .expect("bounded provider listing");
            if let Some(item) = list.data.into_iter().next() {
                break item;
            }
            page = list.next_page.expect("fixture has a public item");
            assert!(page <= 3, "fixture list unavailable");
        };
        let number = sample["number"]
            .as_str()
            .expect("number")
            .parse()
            .expect("decimal");
        let item = source
            .read_item(&allowed, id, kind, number)
            .await
            .expect("item validated against repository");
        assert_eq!(item["id"], sample["id"]);
        assert_eq!(item["number"], sample["number"]);
        assert!(item.get("body").is_some());
        ids.push(json!({"id":item["id"],"number":item["number"]}));
    }
    let report = json!({"provider":provider,"repository_id":id.to_string(),"repository":path,"commit":blob.commit,"path":blob.path,"blob":blob.oid,"file_bytes":blob.bytes.len(),"content_sha256":blob.sha256,"issues_and_changes":ids,"public_anonymous":true});
    println!("Verified {}", report);
}
#[tokio::test]
async fn github_public() {
    source(
        Provider::Github,
        1296269,
        "octocat/Hello-World",
        "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d",
        "README",
    )
    .await;
}
#[tokio::test]
async fn gitlab_public() {
    source(
        Provider::Gitlab,
        34675721,
        "gitlab-org/cli",
        "b923f6be9a6d73112ca53e7a6b7b34153dfd2a16",
        "README.md",
    )
    .await;
}
