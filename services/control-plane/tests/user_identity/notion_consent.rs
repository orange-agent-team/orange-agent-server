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
async fn preserved_legacy_grants() {
    let db =
        Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("disposable database"))
            .await
            .expect("owned source");
    let pool = db.pool();
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("database identity");
    assert!(
        name.strip_prefix("orange_notion_").is_some_and(
            |suffix| suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        )
    );
    sqlx::migrate!()
        .run_to(47, pool)
        .await
        .expect("install schema 47");
    let version: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .expect("actual v47");
    assert_eq!(version, 47);
    let f = fixture_from_store(&db).await;
    let (legacy, _root) = active_run(&f).await;
    let existing = opted_in(&f, &legacy, "allow_external_connectors").await;
    for grant in ["old-active", "old-revoked"] {
        sqlx::query("INSERT INTO repository_source_grants(grant_id,run_id,provider,source_id,source_digest,repository_ids,request_key,request_digest,expires_at) VALUES($1,$2,'github','stored',repeat('a',64),ARRAY['17'],$1,repeat('b',64),clock_timestamp()+interval '1 hour')").bind(grant).bind(&existing).execute(pool).await.expect("prior row");
    }
    sqlx::query("INSERT INTO repository_source_revocations(grant_id) VALUES('old-revoked')")
        .execute(pool)
        .await
        .expect("prior revoke");
    sqlx::query("INSERT INTO repository_source_calls(run_id,provider,grant_id,request_key,request_digest,until_at,finished_at) VALUES($1,'github','old-active','file-request',repeat('c',64),clock_timestamp(),clock_timestamp())").bind(&existing).execute(pool).await.expect("prior call");
    sqlx::query("INSERT INTO repository_file_receipts(artifact_id,run_id,provider,grant_id,request_key,repository_id,path,commit_id,blob_id,size_bytes,content_sha256) VALUES('old-artifact',$1,'github','old-active','file-request','17','doc.txt',repeat('1',40),repeat('2',40),10,repeat('d',64))").bind(&existing).execute(pool).await.expect("prior file reference");
    async fn snapshot(p: &PgPool) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_array((SELECT jsonb_agg(to_jsonb(g) ORDER BY grant_id) FROM repository_source_grants g),(SELECT jsonb_agg(to_jsonb(r) ORDER BY grant_id) FROM repository_source_revocations r),(SELECT jsonb_agg(to_jsonb(c) ORDER BY request_key) FROM repository_source_calls c),(SELECT jsonb_agg(to_jsonb(a) ORDER BY artifact_id) FROM repository_file_receipts a))").fetch_one(p).await.expect("historical rows")
    }
    let before = snapshot(pool).await;
    db.migrate().await.expect("upgrade live DB");
    db.migrate().await.expect("rerun migrations");
    assert_eq!(snapshot(pool).await, before);
    let pins=sqlx::query_as::<_,(String,String,String)>("SELECT provider,action_name,execution_digest FROM connector_release_actions ORDER BY provider,action_name").fetch_all(pool).await.expect("release policy records");
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../config/connector-releases.v1.json"
    ))
    .expect("release catalog");
    assert_eq!(pins.len(), 26);
    for item in manifest["adapters"].as_array().expect("six profiles") {
        for action in item["actions"].as_array().expect("actions") {
            assert!(pins.contains(&(
                item["provider"].as_str().expect("provider").into(),
                action["name"].as_str().expect("name").into(),
                action["execution_digest"].as_str().expect("pin").into()
            )));
        }
    }
    let gh_read = &pins
        .iter()
        .find(|(p, a, _)| p == "github" && a == "read_file")
        .expect("known action")
        .2;
    let allowed: bool =
        sqlx::query_scalar("SELECT connector_grant_allows('github','old-active','read_file',$1)")
            .bind(gh_read)
            .fetch_one(pool)
            .await
            .expect("backfilled release");
    assert!(allowed);
    let write_pin = "0".repeat(64);
    sqlx::query("INSERT INTO connector_releases(provider,release_id,api_revision,allowed_actions) VALUES('github','fixture-future','fixture',$1)").bind(json!({"read_file":gh_read,"comment":write_pin})).execute(pool).await.expect("separate future release");
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT connector_grant_allows('github','old-active','comment',$1)"
        )
        .bind(&write_pin)
        .fetch_one(pool)
        .await
        .expect("future action denied")
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT connector_grant_allows('github','old-active','read_file',$1)"
        )
        .bind(&write_pin)
        .fetch_one(pool)
        .await
        .expect("changed action digest denied")
    );
    sqlx::query("UPDATE connector_releases SET allowed_actions=allowed_actions||jsonb_build_object('comment',$1::text) WHERE provider='github' AND release_id='1.0.0'").bind(&write_pin).execute(pool).await.expect_err("old action set immutable");
    sqlx::query("UPDATE connector_grant_release SET release_id='fixture-future' WHERE provider='github' AND grant_id='old-active'").execute(pool).await.expect_err("rebinding prior grant forbidden");
    let optins: i64 = sqlx::query_scalar("SELECT count(*) FROM cloud_document_source_opt_ins")
        .fetch_one(pool)
        .await
        .expect("no legacy opt-ins");
    assert_eq!(optins, 0);
    let blocked=sqlx::query("INSERT INTO external_source_grants(grant_id,run_id,provider,source_id,source_digest,root_keys,request_key,request_digest,expires_at) VALUES('new',$1,'notion','notion',repeat('a',64),ARRAY['notes'],'k',repeat('a',64),clock_timestamp()+interval '1 hour')").bind(&existing).execute(pool).await.expect_err("old repository consent cannot admit a document grant");
    assert!(blocked.as_database_error().is_some_and(|e|e.message()=="run missing provider-specific external access consent"));
}
#[tokio::test]
async fn notion_egress_grants_and_revoke_during_source_io() {
    preserved_legacy_grants().await;
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
    if let Ok(driver) = std::env::var("ORANGE_NOTION_BROWSER") {
        let result = super::connector_run::browser(&f, &run, driver).await;
        assert_eq!(
            result,
            json!({"searched":true,"page":true,"blocks":true,"database":true,"read_scoped_row":true,"revoked_after_reload":true})
        );
    }
    server.abort();
}
