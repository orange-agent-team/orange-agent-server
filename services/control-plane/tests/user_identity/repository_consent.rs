use super::connector_run::{active_run, keyed};
use super::*;
use orange_control_plane::repo_connector::{
    Provider, Registration, Repository, registry::Registered,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test]
async fn third_party_access_requires_creation_opt_in_and_live_selected_grant() {
    let mut f = fixture().await;
    let outsider = fixture().await;
    let (legacy, _root) = active_run(&f).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("private provider fixture");
    let addr = listener.local_addr().expect("fixture address");
    let calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let count = calls.clone();
    let begin = reached.clone();
    let resume = release.clone();
    let api=Router::new().fallback(axum::routing::any(move |r:Request<Body>|{
        let count=count.clone();let begin=begin.clone();let resume=resume.clone();
        async move{
            count.fetch_add(1,Ordering::SeqCst);
            assert_eq!(r.method(),"GET");assert_eq!(r.headers()["authorization"],"Bearer local-read-token");
            let doc=match r.uri().path(){
                "/repos/acme/notes"=>json!({"id":17,"full_name":"acme/notes","default_branch":"main"}),
                "/api/v4/projects/17"=>json!({"id":17,"path_with_namespace":"acme/notes","default_branch":"main"}),
                "/repos/acme/notes/issues/1"=>{
                    begin.notify_one();resume.notified().await;
                    json!({"id":100,"number":1,"title":"Restricted fixture","state":"open","updated_at":"2026-01-01T00:00:00Z","body":"private fixture bytes","repository_url":format!("http://{addr}/repos/acme/notes")})
                }
                _=>panic!("unexpected fixture request")
            };
            axum::Json(doc)
        }
    }));
    let server =
        tokio::spawn(async move { axum::serve(listener, api).await.expect("serve fixture") });
    for provider in [Provider::Github, Provider::Gitlab] {
        let repo = Repository {
            id: 17,
            path: "acme/notes".into(),
            file_prefixes: vec!["docs".into()],
            issues: true,
            changes: false,
        };
        let registration = Registration {
            provider,
            repositories: vec![repo],
        };
        let reader = registration
            .open_loopback_fixture(addr, "local-read-token")
            .expect("scope");
        f.identity.repository_sources.0.insert(
            provider.name().into(),
            Arc::new(Registered {
                owner: f.user.clone(),
                digest: hex::encode(Sha256::digest(provider.name().as_bytes())),
                registration,
                reader,
            }),
        );
    }
    f.router = auth::routes::router(f.identity.clone());
    let terms = json!({"source_id":"github","source_digest":hex::encode(Sha256::digest(b"github")),"repository_ids":["17"],"duration_seconds":180});
    let old = format!("/api/v1/agent/runs/{legacy}/connectors/github");
    assert_eq!(
        request(&f, "GET", &old, None, headers(&f)).await.1["external_access_selected"],
        false
    );
    assert_eq!(
        request(
            &f,
            "POST",
            &old,
            Some(terms.clone()),
            keyed(&f, "no-opt-in")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Admission uses the real create handler. Only the platform Host's readiness is a fixture here.
    let input = sqlx::query(
        "SELECT root_budget_id,input_artifact_id,input_version_id FROM cloud_runs WHERE run_id=$1",
    )
    .bind(&legacy)
    .fetch_one(&f.pool)
    .await
    .expect("sealed input");
    sqlx::query("INSERT INTO cloud_pool_hosts(host_instance_id,certificate_sha256,profile_id,attestation_digest,region_id,max_workspaces,enabled,credential_expires_at,accepting,live_until,input_protocol_version) VALUES($1,$2,'dev-gvisor-linux-arm64-v1',$3,'dev',8,true,clock_timestamp()+interval '10 minutes',true,clock_timestamp()+interval '10 minutes',1)").bind(Uuid::new_v4().to_string()).bind(hex::encode(Sha256::digest(legacy.as_bytes()))).bind("1".repeat(64)).execute(&f.pool).await.expect("registered host fixture");
    let mut create = json!({"root_budget_id":input.get::<String,_>("root_budget_id"),"input_artifact_id":input.get::<String,_>("input_artifact_id"),"input_version_id":input.get::<String,_>("input_version_id"),"resource_profile_id":"dev-gvisor-linux-arm64-v1","workspace_ttl_seconds":300,"allow_external_connectors":true});
    let (code, result) = request(
        &f,
        "POST",
        "/api/v1/agent/runs",
        Some(create.clone()),
        keyed(&f, "explicit"),
    )
    .await;
    assert!(
        matches!(code, StatusCode::OK | StatusCode::CREATED),
        "{result}"
    );
    let run = result["run_id"].as_str().expect("created run");
    create["allow_external_connectors"] = json!(false);
    assert_eq!(
        request(
            &f,
            "POST",
            "/api/v1/agent/runs",
            Some(create),
            keyed(&f, "explicit")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE cloud_runs SET status='running',version=version+1 WHERE run_id=$1")
        .bind(run)
        .execute(&f.pool)
        .await
        .expect("ready Run fixture");

    let base = format!("/api/v1/agent/runs/{run}/connectors/github");
    let gl = format!("/api/v1/agent/runs/{run}/connectors/gitlab");
    let inventory = request(&f, "GET", &base, None, headers(&f)).await.1;
    assert_eq!(inventory["external_access_selected"], true);
    assert_eq!(
        request(&outsider, "GET", &base, None, headers(&outsider))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let gr = request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "grant")).await;
    assert_eq!(gr.0, StatusCode::OK, "{}", gr.1);
    let id = gr.1["grant_id"].as_str().expect("grant");
    let path = format!("{base}/{id}/actions");
    let list = json!({"action":"list_repositories","page":1,"limit":20});
    assert_eq!(
        request(
            &f,
            "POST",
            &format!("{gl}/{id}/actions"),
            Some(list.clone()),
            keyed(&f, "cross")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for invalid in [
        json!({"action":"list_pull_requests","repository_id":"17","page":1,"limit":1}),
        json!({"action":"read_file","repository_id":"17","revision":"main","path":"other/private"}),
    ] {
        assert_eq!(
            request(&f, "POST", &path, Some(invalid), keyed(&f, "ungranted"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let counted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_source_calls WHERE run_id=$1")
            .bind(run)
            .fetch_one(&f.pool)
            .await
            .expect("call count");
    assert_eq!(counted, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (code, listing) = request(&f, "POST", &path, Some(list.clone()), keyed(&f, "listed")).await;
    assert_eq!(code, StatusCode::OK, "{listing}");
    assert_eq!(listing["data"]["data"][0]["path"], "acme/notes");
    let read = request(
        &f,
        "POST",
        &path,
        Some(json!({"action":"read_issue","repository_id":"17","number":"1"})),
        keyed(&f, "in-flight"),
    );
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .expect("upstream read");
        assert_eq!(
            request(
                &f,
                "DELETE",
                &format!("{base}/{id}"),
                None,
                keyed(&f, "stop")
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        release.notify_one();
    };
    let ((code, denied), ()) = tokio::join!(read, revoke);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(denied.get("data").is_none());
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(
        request(&f, "POST", &path, Some(list), keyed(&f, "old"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), before);
    let mut terms2 = terms;
    terms2["source_id"] = json!("gitlab");
    terms2["source_digest"] = json!(hex::encode(Sha256::digest(b"gitlab")));
    let (_, gr) = request(&f, "POST", &gl, Some(terms2), keyed(&f, "gl")).await;
    let gl_actions = format!(
        "{gl}/{}/actions",
        gr["grant_id"].as_str().expect("gitlab grant")
    );
    let (code, v) = request(
        &f,
        "POST",
        &gl_actions,
        Some(json!({"action":"list_projects","page":1,"limit":1})),
        keyed(&f, "registered"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["data"][0]["id"], "17");
    server.abort();
}
