use super::connector_run::{active_run, keyed};
use super::*;
use orange_control_plane::repo_connector::{
    Provider, Registration, Repository, registry::Registered,
};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

fn register(f: &mut Fixture, addr: std::net::SocketAddr, changes: bool) {
    for provider in [Provider::Github, Provider::Gitlab] {
        let repo = Repository {
            id: 17,
            path: "acme/notes".into(),
            file_prefixes: vec!["docs".into()],
            issues: true,
            changes,
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
                digest: hex::encode(Sha256::digest(if changes {
                    format!("{}-with-changes", provider.name())
                } else {
                    provider.name().to_owned()
                })),
                registration,
                reader,
            }),
        );
    }
    f.router = auth::routes::router(f.identity.clone());
}

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
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let block = gate.clone();
    let count = calls.clone();
    let begin = reached.clone();
    let resume = release.clone();
    let api=Router::new().fallback(axum::routing::any(move |r:Request<Body>|{
        let block=block.clone();let count=count.clone();let begin=begin.clone();let resume=resume.clone();
        async move{
            count.fetch_add(1,Ordering::SeqCst);
            use axum::response::IntoResponse;
            assert!(matches!(r.method().as_str(),"GET"|"HEAD"));assert_eq!(r.headers()["authorization"],"Bearer local-read-token");
            let bytes=b"registered file\n";
            let oid=hex::encode(sha1::Sha1::new().chain_update(format!("blob {}\0",bytes.len())).chain_update(bytes).finalize());
            let commit="1".repeat(40);let tree="2".repeat(40);let child="3".repeat(40);
            let issue=json!({"id":100,"project_id":17,"iid":1,"number":1,"title":"Restricted fixture","state":"open","updated_at":"2026-01-01T00:00:00Z","body":"private fixture bytes","description":"private fixture bytes","repository_url":format!("http://{addr}/repos/acme/notes")});
            let change=json!({"id":101,"target_project_id":17,"base":{"repo":{"id":17}},"number":2,"iid":2,"title":"Review fixture","state":"open","updated_at":"2026-01-01T00:00:00Z","body":"reviewed changes","description":"reviewed changes"});
            let path=r.uri().path();
            let doc=match path{
                "/repos/acme/notes"=>json!({"id":17,"full_name":"acme/notes","default_branch":"main"}),
                "/api/v4/projects/17"=>json!({"id":17,"path_with_namespace":"acme/notes","default_branch":"main"}),
                "/repos/acme/notes/pulls"|"/api/v4/projects/17/merge_requests"=>json!([change]),
                "/repos/acme/notes/pulls/2"|"/api/v4/projects/17/merge_requests/2"=>change,
                "/repos/acme/notes/issues"|"/api/v4/projects/17/issues"=>json!([issue]),
                "/repos/acme/notes/issues/1"|"/api/v4/projects/17/issues/1"=>{
                    if block.swap(false,Ordering::SeqCst){begin.notify_one();resume.notified().await;}
                    issue
                }
                x if x.starts_with("/repos/acme/notes/commits/")=>json!({"sha":commit,"commit":{"tree":{"sha":tree}}}),
                x if x.starts_with("/api/v4/projects/17/repository/commits/")=>json!({"id":commit}),
                x if x==format!("/repos/acme/notes/git/trees/{tree}")=>json!({"sha":tree,"truncated":false,"tree":[{"path":"docs","mode":"040000","type":"tree","sha":child}]}),
                x if x==format!("/repos/acme/notes/git/trees/{child}")=>json!({"sha":child,"truncated":false,"tree":[{"path":"scope.txt","mode":"100644","type":"blob","sha":oid,"size":bytes.len()}]}),
                "/api/v4/projects/17/repository/tree"=>json!([{"path":"docs/scope.txt","mode":"100644","type":"blob","id":oid}]),
                "/api/v4/projects/17/repository/files/docs%2Fscope.txt" if r.method()=="HEAD"=>{
                    return ([("x-gitlab-commit-id",commit),("x-gitlab-file-path","docs/scope.txt".to_owned()),("x-gitlab-blob-id",oid),("x-gitlab-content-sha256",hex::encode(Sha256::digest(bytes))),("x-gitlab-size",bytes.len().to_string())],"").into_response();
                },
                x if x==format!("/repos/acme/notes/git/blobs/{oid}")||x==format!("/api/v4/projects/17/repository/blobs/{oid}/raw")=>return bytes.as_slice().into_response(),
                _=>panic!("unexpected fixture request: {path}")
            };
            axum::Json(doc).into_response()
        }
    }));
    let server =
        tokio::spawn(async move { axum::serve(listener, api).await.expect("serve fixture") });
    register(&mut f, addr, false);
    let catalog = "/api/v1/agent/connector-tools";
    let historic = request(&f, "GET", catalog, None, headers(&f)).await;
    assert_eq!(historic.0, StatusCode::OK);
    assert!(
        historic.1["tools"]
            .as_array()
            .expect("legacy catalog")
            .iter()
            .all(|t| t["connector"] != "github" && t["connector"] != "gitlab")
    );
    let opted_catalog = format!("{catalog}?repository_listing=true");
    let (status, listing) = request(&f, "GET", &opted_catalog, None, headers(&f)).await;
    assert_eq!(status, StatusCode::OK);
    for (family, name) in [("github", "list_repositories"), ("gitlab", "list_projects")] {
        let tools = listing["tools"].as_array().expect("catalog");
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .expect("registered tool");
        assert_eq!(tool["connector"], family);
        assert_eq!(tool["approval_required"], true);
        assert_eq!(tool["model_definition"]["name"], name);
    }
    let foreign = request(&outsider, "GET", &opted_catalog, None, headers(&outsider)).await;
    assert_eq!(foreign.0, StatusCode::OK);
    assert!(
        foreign.1["tools"]
            .as_array()
            .expect("foreign catalog")
            .iter()
            .all(|t| t["connector"] != "github" && t["connector"] != "gitlab")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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

    let created = super::connector_run::opted_in(&f, &legacy, "allow_external_connectors").await;
    let run = created.as_str();
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
    if let Ok(driver) = std::env::var("ORANGE_REPOSITORY_BROWSER") {
        register(&mut f, addr, true);
        let evidence = super::connector_run::browser(&f, run, driver).await;
        assert_eq!(
            evidence,
            json!({"providers":["github","gitlab"],"read_issue":true,"read_change":true,"file_verified":true,"revoked_after_reload":true})
        );
    }
    server.abort();
}
