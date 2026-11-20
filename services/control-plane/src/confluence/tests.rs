use super::fixtures::*;
use super::*;
use std::sync::atomic::Ordering;
fn a(v: Value) -> Action {
    serde_json::from_value(v).expect("request")
}
async fn doc(f: &Fixture, v: Value) -> Result<Value, Error> {
    match f.src.execute(&f.scope, a(v), || async { Ok(()) }).await? {
        ResultData::Document(v) => Ok(v["data"].clone()),
        _ => panic!("expected document"),
    }
}
#[tokio::test]
async fn tenant_scoped_pages_and_credentialless_external_delivery() {
    let f = fixture().await;
    let query = json!({"action":"search_pages","space":"ops","query":"release","limit":3});
    let list = doc(&f, query.clone()).await.expect("search");
    let page = &list["pages"][0]["handle"];
    assert!(page.is_string());
    let mut next = query.clone();
    next["cursor"] = list["next_cursor"].clone();
    assert_eq!(doc(&f, next).await.expect("more")["pages"], json!([]));
    let body = doc(&f, json!({"action":"read_page","space":"ops","page":page}))
        .await
        .expect("page");
    assert_eq!(body["text"], "Visible page text[mention]\n");
    assert_eq!(body["omitted_nodes"], json!(["extension"]));
    let file = &body["attachments"][0]["handle"];
    let kids = doc(
        &f,
        json!({"action":"list_children","space":"ops","page":page,"limit":2}),
    )
    .await
    .expect("children");
    assert_eq!(kids["children"][0]["id"], "12");
    let read = json!({"action":"read_attachment","space":"ops","attachment":file});
    let ResultData::Attachment { bytes, sha256, .. } = f
        .src
        .execute(&f.scope, a(read.clone()), || async { Ok(()) })
        .await
        .expect("attachment")
    else {
        panic!("file")
    };
    assert_eq!(bytes.len(), SIZE);
    assert!(bytes.iter().all(|b| *b == 0x43));
    assert_eq!(sha256, hex::encode(Sha256::digest(vec![0x43u8; SIZE])));
    assert_eq!(f.media.load(Ordering::SeqCst), 1);
    f.bad_redirect.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.src
            .execute(&f.scope, a(read.clone()), || async { Ok(()) })
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(f.media.load(Ordering::SeqCst), 1);
    f.moved.store(true, Ordering::SeqCst);
    assert!(matches!(
        doc(&f, json!({"action":"read_page","space":"ops","page":page})).await,
        Err(Error::Changed)
    ));
    let n = f.hits.load(Ordering::SeqCst);
    let other = Scope {
        grant: "another".into(),
        spaces: f.scope.spaces.clone(),
    };
    assert!(matches!(
        f.src.execute(&other, a(read), || async { Ok(()) }).await,
        Err(Error::Denied)
    ));
    assert_eq!(f.hits.load(Ordering::SeqCst), n);
    f.tenant.store(false, Ordering::SeqCst);
    assert!(matches!(doc(&f, query).await, Err(Error::Changed)));
    assert_eq!(f.hits.load(Ordering::SeqCst), n + 1);
}
