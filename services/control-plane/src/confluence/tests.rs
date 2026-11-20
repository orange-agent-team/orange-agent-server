use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    routing::any,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
const CLOUD: &str = "88888888-8888-4888-8888-888888888888";
const PAYLOAD: &[u8] = b"opaque test attachment";
struct Fixture {
    src: Source,
    scope: Scope,
    hits: Arc<AtomicUsize>,
    media: Arc<AtomicUsize>,
    bad_redirect: Arc<AtomicBool>,
    moved: Arc<AtomicBool>,
    tenant: Arc<AtomicBool>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for s in &self.servers {
            s.abort();
        }
    }
}
fn page(id: &str, parent: &str, space: &str) -> Value {
    let adf = json!({"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Visible page text","marks":[{"type":"link","attrs":{"href":"https://private.invalid"}}]},{"type":"mention","attrs":{"id":"private-id"}}]},{"type":"extension","attrs":{"private":"secret"}}]});
    json!({"id":id,"title":format!("Page {id}"),"spaceId":space,"parentId":parent,"status":"current","version":{"number":3},"body":{"atlas_doc_format":{"representation":"atlas_doc_format","value":adf.to_string()}}})
}
fn meta() -> Value {
    json!({"id":"att90","fileId":"f-71","pageId":"11","title":"report.txt","mediaType":"text/plain","fileSize":PAYLOAD.len(),"version":{"number":2},"status":"current"})
}
async fn fixture() -> Fixture {
    let primary = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("site");
    let delivery = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("media");
    let site = primary.local_addr().expect("address");
    let cdn = delivery.local_addr().expect("address");
    let media = Arc::new(AtomicUsize::new(0));
    let send = media.clone();
    let cdn_task = tokio::spawn(async move {
        axum::serve(
            delivery,
            Router::new().fallback(any(move |r: Request<Body>| {
                let n = send.clone();
                async move {
                    assert!(r.headers().get("authorization").is_none());
                    assert!(r.headers().get("cookie").is_none());
                    assert_eq!(r.uri().path(), "/fixture-signed");
                    n.fetch_add(1, Ordering::SeqCst);
                    PAYLOAD
                }
            })),
        )
        .await
        .expect("serve media")
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let bad_redirect = Arc::new(AtomicBool::new(false));
    let moved = Arc::new(AtomicBool::new(false));
    let tenant = Arc::new(AtomicBool::new(true));
    let (count, bad, move_page, site_ok) = (
        hits.clone(),
        bad_redirect.clone(),
        moved.clone(),
        tenant.clone(),
    );
    let app=Router::new().fallback(any(move|r:Request<Body>|{
        let(c,b,m,t)=(count.clone(),bad.clone(),move_page.clone(),site_ok.clone());
        async move{
            c.fetch_add(1,Ordering::SeqCst);assert_eq!(r.method(),"GET");
            let path=r.uri().path();
            if path=="/_edge/tenant_info"{assert!(r.headers().get("authorization").is_none());return Json(json!({"cloudId":if t.load(Ordering::SeqCst){CLOUD}else{"different"}})).into_response();}
            assert_eq!(r.headers()["authorization"],"Bearer fixture-confluence-token");
            let value=match path{
                "/wiki/api/v2/spaces/7"=>json!({"id":"7","key":"OPS","status":"current"}),
                "/wiki/rest/api/search"=>{
                    let u=reqwest::Url::parse(&format!("http://{site}{}",r.uri())).expect("request");
                    let args:BTreeMap<_,_>=u.query_pairs().collect();assert!(args["cql"].starts_with("type = page AND space = \"OPS\""));assert_eq!(args["expand"],"content.space");
                    if args.contains_key("cursor"){json!({"results":[],"_links":{}})}else{json!({"results":[{"content":{"id":"11","type":"page","space":{"key":"OPS","id":7}}}],"_links":{"next":"/wiki/rest/api/search?cursor=wire_cursor"}})}
                }
                "/wiki/api/v2/pages/11"=>page("11","1",if m.load(Ordering::SeqCst){"8"}else{"7"}),
                "/wiki/api/v2/pages/12"=>page("12","11","7"),
                "/wiki/api/v2/pages/11/direct-children"=>json!({"results":[{"id":"12","type":"page","title":"Page 12"}],"_links":{}}),
                "/wiki/api/v2/pages/11/attachments"=>json!({"results":[meta()],"_links":{}}),
                "/wiki/api/v2/attachments/att90"=>meta(),
                "/wiki/rest/api/content/11/child/attachment/att90/download"=>{
                    assert_eq!(r.uri().query(),Some("version=2"));
                    let location=if b.load(Ordering::SeqCst){format!("http://{site}/unexpected")}else{format!("http://{cdn}/fixture-signed?grant=one-time")};
                    return Response::builder().status(302).header("location",location).body(Body::empty()).expect("redirect");
                }
                _=>panic!("unexpected fixture route {path}"),
            };Json(value).into_response()
        }
    }));
    let api_task =
        tokio::spawn(async move { axum::serve(primary, app).await.expect("serve site") });
    let reg = Registration {
        tenant: "example.atlassian.net".into(),
        cloud_id: CLOUD.into(),
        spaces: BTreeMap::from([(
            "ops".into(),
            Space {
                id: "7".into(),
                key: "OPS".into(),
            },
        )]),
        media_hosts: BTreeSet::new(),
    };
    let src = reg
        .open_loopback_fixture(
            site,
            cdn,
            &Credential::CloudOauth {
                token: "fixture-confluence-token".into(),
            },
        )
        .expect("source");
    Fixture {
        src,
        scope: Scope {
            grant: "active".into(),
            spaces: BTreeSet::from(["ops".into()]),
        },
        hits,
        media,
        bad_redirect,
        moved,
        tenant,
        servers: vec![api_task, cdn_task],
    }
}
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
    assert_eq!(bytes, PAYLOAD);
    assert_eq!(sha256, hex::encode(Sha256::digest(PAYLOAD)));
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
