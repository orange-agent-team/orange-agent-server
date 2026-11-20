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
pub const CLOUD: &str = "88888888-8888-4888-8888-888888888888";
pub const SIZE: usize = 3 * 1024 * 1024;
pub struct Fixture {
    pub site: std::net::SocketAddr,
    pub media_addr: std::net::SocketAddr,
    pub hold_media: Arc<AtomicBool>,
    pub media_reached: Arc<tokio::sync::Notify>,
    pub media_release: Arc<tokio::sync::Notify>,
    pub src: Source,
    pub scope: Scope,
    pub hits: Arc<AtomicUsize>,
    pub media: Arc<AtomicUsize>,
    pub bad_redirect: Arc<AtomicBool>,
    pub moved: Arc<AtomicBool>,
    pub tenant: Arc<AtomicBool>,
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
    json!({"id":"att90","fileId":"f-71","pageId":"11","title":"report.txt","mediaType":"text/plain","fileSize":SIZE,"version":{"number":2},"status":"current"})
}
pub async fn fixture() -> Fixture {
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
    let hold_media = Arc::new(AtomicBool::new(false));
    let media_reached = Arc::new(tokio::sync::Notify::new());
    let media_release = Arc::new(tokio::sync::Notify::new());
    let (gate, reached, released) = (
        hold_media.clone(),
        media_reached.clone(),
        media_release.clone(),
    );
    let cdn_task = tokio::spawn(async move {
        axum::serve(
            delivery,
            Router::new().fallback(any(move |r: Request<Body>| {
                let (n, g, rdy, done) = (
                    send.clone(),
                    gate.clone(),
                    reached.clone(),
                    released.clone(),
                );
                async move {
                    assert!(r.headers().get("authorization").is_none());
                    assert!(r.headers().get("cookie").is_none());
                    assert_eq!(r.uri().path(), "/fixture-signed");
                    n.fetch_add(1, Ordering::SeqCst);
                    let data = vec![0x43u8; SIZE];
                    if g.swap(false, Ordering::SeqCst) {
                        rdy.notify_one();
                        done.notified().await;
                    }
                    data
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
        site,
        media_addr: cdn,
        hold_media,
        media_reached,
        media_release,
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
impl Fixture {
    pub fn registration(&self) -> Registration {
        self.src.registration.clone()
    }
}
