use super::*;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    response::IntoResponse,
    routing::any,
};
use std::sync::{Arc, atomic::AtomicUsize};
const PAGE: &str = "00000000-0000-4000-8000-000000000001";
const SECOND: &str = "00000000-0000-4000-8000-000000000002";
const DB: &str = "00000000-0000-4000-8000-000000000003";
const DS: &str = "00000000-0000-4000-8000-000000000004";
const ROW: &str = "00000000-0000-4000-8000-000000000005";
const CHILD: &str = "00000000-0000-4000-8000-000000000006";
const SYNCED: &str = "00000000-0000-4000-8000-000000000007";
const NESTED: &str = "00000000-0000-4000-8000-000000000008";
const EDIT: &str = "2026-11-01T00:00:00.000Z";
struct Fixture {
    source: Source,
    calls: Arc<AtomicUsize>,
    moved: Arc<AtomicBool>,
    response: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn page(id: &str, parent: Value) -> Value {
    json!({"object":"page","id":id,"in_trash":false,"parent":parent,"last_edited_time":EDIT,"properties":{
        "Name":{"type":"title","title":[{"type":"text","plain_text":"项目笔记"}]},
        "Done":{"type":"checkbox","checkbox":false},
        "Related":{"type":"relation","relation":[{"id":SECOND}]}
    }})
}
fn block(id: &str, kind: &str, parent: Value, children: bool) -> Value {
    let mut v = json!({"object":"block","type":kind,"id":id,"parent":parent,"has_children":children,"in_trash":false,"last_edited_time":EDIT});
    v[kind] = if kind == "synced_block" {
        json!({"synced_from":{"type":"block_id","block_id":SECOND}})
    } else {
        json!({"rich_text":[{"type":"text","plain_text":"local content","href":"https://example.invalid"}]})
    };
    v
}
async fn fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("private listener");
    let origin = Url::parse(&format!(
        "http://{}/v1",
        listener.local_addr().expect("address")
    ))
    .expect("url");
    let count = Arc::new(AtomicUsize::new(0));
    let moved = Arc::new(AtomicBool::new(false));
    let status = Arc::new(AtomicUsize::new(200));
    let c = count.clone();
    let m = moved.clone();
    let st = status.clone();
    let app=Router::new().fallback(any(move|req:Request<Body>|{
        let c=c.clone();let m=m.clone();let st=st.clone();
        async move{
            c.fetch_add(1,Ordering::SeqCst);
            assert_eq!(req.headers()["authorization"],"Bearer fixture-private-source");
            assert_eq!(req.headers()["notion-version"],VERSION);
            let status=StatusCode::from_u16(st.load(Ordering::SeqCst) as u16).expect("fixture status");
            if status!=StatusCode::OK{return (status,[("retry-after","10")],Json(json!({"object":"error"}))).into_response();}
            let path=req.uri().path().to_owned();
            let value=if path==format!("/v1/pages/{PAGE}")||path==format!("/v1/pages/{SECOND}"){
                page(path.rsplit('/').next().expect("id"),json!({"type":"workspace","workspace":true}))
            }else if path==format!("/v1/pages/{ROW}"){page(ROW,json!({"type":"data_source_id","data_source_id":if m.load(Ordering::SeqCst){SECOND}else{DS},"database_id":DB}))}
            else if path==format!("/v1/blocks/{PAGE}/children"){
                let next=req.uri().query().unwrap_or("").contains("start_cursor=next");
                json!({"object":"list","has_more":!next,"next_cursor":if next{Value::Null}else{json!("next")},"results":if next{vec![block(SYNCED,"synced_block",json!({"type":"page_id","page_id":PAGE}),true)]}else{vec![block(CHILD,"toggle",json!({"type":"page_id","page_id":PAGE}),true)]}})
            }else if path==format!("/v1/blocks/{CHILD}"){
                block(CHILD,"toggle",json!({"type":"page_id","page_id":if m.load(Ordering::SeqCst){SECOND}else{PAGE}}),true)
            }else if path==format!("/v1/blocks/{CHILD}/children"){
                json!({"object":"list","has_more":false,"next_cursor":null,"results":[block(NESTED,"paragraph",json!({"type":"block_id","block_id":CHILD}),false)]})
            }else if path==format!("/v1/databases/{DB}"){
                json!({"object":"database","id":DB,"in_trash":false,"data_sources":[{"id":DS,"name":"Tasks"}]})
            }else if path==format!("/v1/data_sources/{DS}/query"){
                assert_eq!(req.method(),"POST");
                let body:Value=serde_json::from_slice(&to_bytes(req.into_body(),4096).await.expect("body")).expect("json");
                assert_eq!(body,json!({"page_size":3,"filter":{"property":"Done","checkbox":{"equals":false}}}));
                json!({"object":"list","has_more":false,"next_cursor":null,"results":[page(ROW,json!({"type":"data_source_id","data_source_id":DS,"database_id":DB}))]})
            }else{panic!("unregistered fixture route {path}")};
            Json(value).into_response()
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let spec = Registration {
        roots: BTreeMap::from([
            (
                "a".into(),
                Root::Page {
                    page_id: PAGE.into(),
                },
            ),
            (
                "b".into(),
                Root::Page {
                    page_id: SECOND.into(),
                },
            ),
            (
                "db".into(),
                Root::Database {
                    database_id: DB.into(),
                    data_sources: vec![DS.into()],
                },
            ),
        ]),
    };
    Fixture {
        source: spec
            .build("fixture-private-source", origin)
            .expect("registered"),
        calls: count,
        moved,
        response: status,
        server,
    }
}
fn scope() -> Scope {
    Scope {
        grant: "grant-a".into(),
        allowed: BTreeSet::from(["a".into(), "b".into(), "db".into()]),
    }
}
fn decode(v: Value) -> Action {
    serde_json::from_value(v).expect("typed request")
}
#[tokio::test]
async fn four_actions_bound_to_granted_roots_and_live_parentage() {
    let f = fixture().await;
    let s = scope();
    let exec = |a: Value| f.source.execute(&s, decode(a));
    let found = exec(json!({"action":"search_pages","query":"笔记","limit":1}))
        .await
        .expect("scoped title search");
    assert_eq!(found["data"]["pages"][0]["root"], "a");
    assert!(
        found["data"]["pages"][0]["page"]
            .get("properties")
            .is_none()
    );
    let cursor = &found["data"]["next_cursor"];
    let hits = f.calls.load(Ordering::SeqCst);
    assert!(matches!(
        exec(json!({"action":"search_pages","query":"different","limit":1,"cursor":cursor})).await,
        Err(Error::Denied)
    ));
    assert_eq!(f.calls.load(Ordering::SeqCst), hits);
    let second = exec(json!({"action":"search_pages","query":"笔记","limit":1,"cursor":cursor}))
        .await
        .expect("next registered page");
    assert_eq!(second["data"]["pages"][0]["root"], "b");
    let read = exec(json!({"action":"read_page","root":"a"}))
        .await
        .expect("page");
    assert_eq!(
        read["data"]["properties"]["Related"],
        json!({"type":"relation","projected":false})
    );
    let first = exec(json!({"action":"list_blocks","root":"a","limit":1}))
        .await
        .expect("direct blocks");
    let handle = &first["data"]["blocks"][0]["children"];
    let more = exec(
        json!({"action":"list_blocks","root":"a","limit":1,"cursor":first["data"]["next_cursor"]}),
    )
    .await
    .expect("second page");
    assert!(
        more["data"]["blocks"][0].get("children").is_none(),
        "synced content cannot extend the root"
    );
    let nested = exec(json!({"action":"list_blocks","root":"a","limit":1,"target":handle}))
        .await
        .expect("nested local block");
    assert_eq!(nested["data"]["blocks"][0]["text"], "local content");
    let other = Scope {
        grant: "different-grant".into(),
        allowed: s.allowed.clone(),
    };
    let hits = f.calls.load(Ordering::SeqCst);
    assert!(matches!(
        f.source
            .execute(
                &other,
                decode(json!({"action":"list_blocks","root":"a","limit":1,"target":handle}))
            )
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(f.calls.load(Ordering::SeqCst), hits);
    let db=exec(json!({"action":"query_database","root":"db","data_source":DS,"limit":3,"filter":{"property":"Done","kind":"checkbox","equals":false}})).await.expect("data source query");
    let row = &db["data"]["rows"][0]["handle"];
    assert_eq!(
        exec(json!({"action":"read_page","root":"db","page":row}))
            .await
            .expect("scoped database row")["data"]["page_id"],
        ROW
    );
    f.moved.store(true, Ordering::SeqCst);
    assert!(matches!(
        exec(json!({"action":"list_blocks","root":"a","limit":1,"target":handle})).await,
        Err(Error::Changed)
    ));
    assert!(matches!(
        exec(json!({"action":"read_page","root":"db","page":row})).await,
        Err(Error::Changed)
    ));
}
#[tokio::test]
async fn refusals_cooldowns_and_invalid_query_never_return_partial_data() {
    for (status, want_refused) in [(401, true), (429, false)] {
        let f = fixture().await;
        let s = scope();
        assert!(matches!(f.source.execute(&s,decode(json!({"action":"query_database","root":"db","data_source":DS,"limit":1,"filter":{"property":"Text","kind":"formula","equals":"x"}}))).await,Err(Error::Invalid)));
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        f.response.store(status, Ordering::SeqCst);
        for _ in 0..2 {
            assert!(
                f.source
                    .execute(&s, decode(json!({"action":"read_page","root":"a"})))
                    .await
                    .is_err()
            );
        }
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.source.authorization_refused(), want_refused);
    }
}
