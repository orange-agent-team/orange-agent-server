use super::connector_run::{active_run, keyed, opted_in};
use super::*;
use orange_control_plane::notion::{Registration, Root, VERSION, registry::Registered};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
const PAGE: &str = "00000000-0000-4000-8000-000000000011";
const DATABASE: &str = "00000000-0000-4000-8000-000000000012";
const DS: &str = "00000000-0000-4000-8000-000000000013";
const ROW: &str = "00000000-0000-4000-8000-000000000014";
const BLOCK: &str = "00000000-0000-4000-8000-000000000015";
const EDIT: &str = "2026-11-01T00:00:00.000Z";
fn notion_page(id: &str, title: &str, parent: Value) -> Value {
    json!({"object":"page","id":id,"in_trash":false,"last_edited_time":EDIT,"parent":parent,"properties":{"Name":{"type":"title","title":[{"type":"text","plain_text":title}]},"Status":{"type":"select","select":{"name":"Open"}}}})
}
#[tokio::test]
async fn notion_egress_grants_and_revoke_during_source_io() {
    let mut f = fixture().await;
    let other = fixture().await;
    let (legacy, _root) = active_run(&f).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local fixture");
    let addr = listener.local_addr().expect("address");
    let count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(AtomicBool::new(false));
    let reached = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let (c, hold, begin, resume) = (
        count.clone(),
        gate.clone(),
        reached.clone(),
        released.clone(),
    );
    let app=Router::new().fallback(axum::routing::any(move|r:Request<Body>|{
        let (c,hold,begin,resume)=(c.clone(),hold.clone(),begin.clone(),resume.clone());
        async move{
            c.fetch_add(1,Ordering::SeqCst);
            assert_eq!(r.headers()["notion-version"],VERSION);assert_eq!(r.headers()["authorization"],"Bearer local-notion-token");
            let path=r.uri().path().to_string();
            let row=notion_page(ROW,"Approved record",json!({"type":"data_source_id","data_source_id":DS,"database_id":DATABASE}));
            let doc=if path==format!("/v1/pages/{PAGE}"){
                if hold.swap(false,Ordering::SeqCst){begin.notify_one();resume.notified().await;}
                notion_page(PAGE,"Shared notes",json!({"type":"workspace","workspace":true}))
            }else if path==format!("/v1/pages/{ROW}"){row}
            else if path==format!("/v1/databases/{DATABASE}"){json!({"object":"database","id":DATABASE,"in_trash":false,"data_sources":[{"id":DS,"name":"Tasks"}]})}
            else if path==format!("/v1/data_sources/{DS}/query"){
                assert_eq!(r.method(),"POST");json!({"object":"list","has_more":false,"next_cursor":null,"results":[row]})
            }else if path==format!("/v1/blocks/{PAGE}/children"){
                json!({"object":"list","has_more":false,"next_cursor":null,"results":[{"object":"block","id":BLOCK,"parent":{"type":"page_id","page_id":PAGE},"in_trash":false,"type":"paragraph","has_children":false,"last_edited_time":EDIT,"paragraph":{"rich_text":[{"type":"text","plain_text":"fixture block contents"}]}}]})
            }else{panic!("unregistered source route {path}")};
            axum::Json(doc)
        }
    }));
    let server =
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve fixture") });
    let reg = Registration {
        roots: BTreeMap::from([
            (
                "notes".into(),
                Root::Page {
                    page_id: PAGE.into(),
                },
            ),
            (
                "tasks".into(),
                Root::Database {
                    database_id: DATABASE.into(),
                    data_sources: vec![DS.into()],
                },
            ),
        ]),
    };
    let source = reg
        .open_loopback_fixture(addr, "local-notion-token")
        .expect("reader");
    f.identity.notion_sources.0.insert(
        "shared".into(),
        Arc::new(Registered {
            owner: f.user.clone(),
            digest: "d".repeat(64),
            registration: reg,
            reader: source,
        }),
    );
    f.router = auth::routes::router(f.identity.clone());
    let terms = json!({"source_id":"shared","source_digest":"d".repeat(64),"root_keys":["notes","tasks"],"duration_seconds":180});
    let repo_only = opted_in(&f, &legacy, "allow_external_connectors").await;
    for run in [&legacy, &repo_only] {
        let url = format!("/api/v1/agent/runs/{run}/connectors/notion");
        let inventory = request(&f, "GET", &url, None, headers(&f)).await.1;
        assert_eq!(inventory["document_access_selected"], false);
        assert_eq!(
            request(
                &f,
                "POST",
                &url,
                Some(terms.clone()),
                keyed(&f, "before-opt-in")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let run = opted_in(&f, &legacy, "allow_document_connectors").await;
    let base = format!("/api/v1/agent/runs/{run}/connectors/notion");
    let missing = request(&other, "GET", &base, None, headers(&other)).await.0;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    let repo = format!("/api/v1/agent/runs/{run}/connectors/github");
    assert_eq!(
        request(&f, "GET", &repo, None, headers(&f)).await.1["external_access_selected"],
        false
    );
    let (code, rec) = request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve")).await;
    assert_eq!(code, StatusCode::OK, "{rec}");
    let grant = format!("{base}/{}", rec["grant_id"].as_str().expect("grant"));
    let actions = format!("{grant}/actions");
    let wrong = json!({"action":"read_page","root":"not-granted"});
    assert_eq!(
        request(&f, "POST", &actions, Some(wrong), keyed(&f, "wrong"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let query = json!({"action":"query_database","root":"tasks","data_source":DS,"limit":3});
    let (code, data) = request(
        &f,
        "POST",
        &actions,
        Some(query.clone()),
        keyed(&f, "query"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{data}");
    let handle = &data["data"]["rows"][0]["handle"];
    assert!(handle.is_string());
    let row = json!({"action":"read_page","root":"tasks","page":handle});
    assert_eq!(
        request(&f, "POST", &actions, Some(row.clone()), keyed(&f, "row"))
            .await
            .1["data"]["title"],
        "Approved record"
    );
    gate.store(true, Ordering::SeqCst);
    let before = count.load(Ordering::SeqCst);
    let read = request(
        &f,
        "POST",
        &actions,
        Some(json!({"action":"read_page","root":"notes"})),
        keyed(&f, "in-flight"),
    );
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("upstream read");
        assert_eq!(
            request(&f, "DELETE", &grant, None, keyed(&f, "revoke"))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        released.notify_one();
    };
    let ((code, v), ()) = tokio::join!(read, revoke);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(v.get("data").is_none());
    assert_eq!(
        count.load(Ordering::SeqCst),
        before + 1,
        "no second provider request after revoke"
    );
    let newer = request(&f, "POST", &base, Some(terms), keyed(&f, "again"))
        .await
        .1;
    let path = format!(
        "{base}/{}/actions",
        newer["grant_id"].as_str().expect("new grant")
    );
    assert_eq!(
        request(&f, "POST", &path, Some(row), keyed(&f, "old-handle"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(count.load(Ordering::SeqCst), before + 1);
    server.abort();
}
