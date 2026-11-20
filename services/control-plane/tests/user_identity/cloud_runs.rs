//! Actual local mTLS, PG, user HTTP and OpenDAL. Host admission and its cleanup
//! statement are explicit fixtures; this test does not execute a user Agent.
use super::*;
use orange_control_plane::{
    artifacts::ArtifactStore,
    cloud_host::{self, HostTls},
};
use orange_protocol::{
    artifacts::CreateUpload,
    grpc::host::{self, platform_host_pool_client::PlatformHostPoolClient},
};
use sha2::{Digest, Sha256};
use std::path::Path as FsPath;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
const API: &str = "/api/v1/agent/runs";
const PROFILE: &str = "dev-gvisor-linux-arm64-v1";
fn tls_file(dir: &FsPath, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).expect("private PKI fixture")
}
async fn client(
    dir: &FsPath,
    port: u16,
    identity: &str,
) -> Result<PlatformHostPoolClient<Channel>, tonic::transport::Error> {
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(tls_file(dir, "host-ca.pem")))
        .identity(Identity::from_pem(
            tls_file(dir, &format!("{identity}.pem")),
            tls_file(dir, &format!("{identity}.key")),
        ));
    let channel = Channel::from_shared(format!("https://127.0.0.1:{port}"))
        .expect("url")
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(PlatformHostPoolClient::new(channel))
}
fn keyed(f: &Fixture, key: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", key.parse().expect("key"));
    h
}
#[tokio::test]
async fn platform_only_admission_fences_old_leases_and_holds_capacity_until_cleanup() {
    let f = fixture().await;
    let other = fixture().await;
    let root = tempfile::tempdir().expect("object root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private object root");
    }
    let store = ArtifactStore::open_local(f.pool.clone(), root.path(), 1048576, 100)
        .await
        .expect("actual OpenDAL");
    let bytes = b"Summarize this project.".to_vec();
    let upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        media_type: "text/plain".to_owned(),
        size_bytes: bytes.len().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
    };
    let actor = f
        .identity
        .artifact_actor(&headers(&f), true)
        .await
        .expect("user actor");
    store
        .begin_upload(&actor, &f.user, &upload)
        .await
        .expect("upload reservation");
    let input = store
        .put(
            actor,
            f.user.clone(),
            upload.upload_id,
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("actual input object");
    let (s, budget) = request(
        &f,
        "POST",
        "/api/v1/account/budgets",
        Some(json!({"scope_id":"agent","limit":"50000"})),
        keyed(&f, "budget"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{budget}");
    let body = json!({"root_budget_id":budget["root_budget_id"],"input_artifact_id":input.artifact_id,"input_version_id":input.version_id,
        "resource_profile_id":PROFILE,"workspace_ttl_seconds":300});
    assert_eq!(
        request(&f, "POST", API, Some(body.clone()), keyed(&f, "initial"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&other, "POST", API, Some(body.clone()), headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let mut foreign = body.clone();
    foreign["owner_id"] = json!(other.user);
    assert_eq!(
        request(&f, "POST", API, Some(foreign), headers(&f)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut no_csrf = headers(&f);
    no_csrf.remove("x-csrf-token");
    assert_eq!(
        request(&f, "POST", API, Some(body.clone()), no_csrf)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let tls_path = std::env::var("ORANGE_TEST_HOST_TLS").expect("isolated Host PKI");
    let tls = FsPath::new(&tls_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("dedicated gateway");
    let port = listener.local_addr().expect("local port").port();
    let creds = HostTls {
        server_certificate_pem: tls_file(tls, "gateway.pem"),
        server_private_key_pem: tls_file(tls, "gateway.key"),
        platform_host_ca_pem: tls_file(tls, "host-ca.pem"),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let db = f.pool.clone();
    let service = tokio::spawn(cloud_host::serve(listener, db, creds, async {
        let _ = stopped.await;
    }));
    let offline = request(
        &f,
        "GET",
        "/api/v1/agent/workspace-capacity",
        None,
        headers(&f),
    )
    .await;
    assert_eq!(offline.1["can_reserve"], false);
    let mut host = client(tls, port, "host").await.expect("platform TLS");
    let host_id = Uuid::new_v4().to_string();
    let mut attestation = hash_secret("explicit isolated host profile admission fixture");
    let mut announce = host::HostPollRequest {
        host_instance_id: host_id.clone(),
        resource_profile_id: PROFILE.into(),
        attestation_digest: attestation.clone(),
        accept_reservations: true,
    };
    assert_eq!(
        host.poll(announce.clone())
            .await
            .expect_err("unadmitted")
            .code(),
        tonic::Code::PermissionDenied
    );
    if let Ok(database_file) = std::env::var("ORANGE_CLOUD_QUALIFIED_DATABASE_FILE") {
        let script =
            FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/admit-local-cloud-host.py");
        let result = tokio::process::Command::new(
            std::env::var("ORANGE_CLOUD_QA_PYTHON").expect("test Python"),
        )
        .arg(script)
        .arg("--database-url-file")
        .arg(database_file)
        .arg("--host-cert")
        .arg(tls.join("host.pem"))
        .arg("--platform-host-ca")
        .arg(tls.join("host-ca.pem"))
        .arg("--host-id")
        .arg(&host_id)
        .output()
        .await
        .expect("actual local operator admission");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = std::str::from_utf8(&result.stdout).expect("operator report");
        let receipt: Value = serde_json::from_str(text.lines().last().expect("admission receipt"))
            .expect("receipt JSON");
        assert_eq!(receipt["host_instance_id"], host_id);
        attestation = receipt["attestation_digest"]
            .as_str()
            .expect("proof")
            .to_owned();
        if let Ok(path) = std::env::var("ORANGE_CLOUD_WEB_EVIDENCE") {
            std::fs::write(
                FsPath::new(&path).join("host-registration.json"),
                serde_json::to_vec_pretty(&receipt).expect("registration receipt"),
            )
            .expect("operator evidence");
        }
        announce.attestation_digest = attestation.clone();
        println!("PASS development Host registration after actual pinned OCI qualification");
    } else {
        sqlx::query("INSERT INTO cloud_pool_hosts(host_instance_id,certificate_sha256,profile_id,attestation_digest,region_id,max_workspaces,credential_expires_at)
        VALUES($1,$2,$3,$4,'dev-local',1,clock_timestamp()+interval '1 hour')")
        .bind(&host_id).bind(hex::encode(Sha256::digest(tls_file(tls,"host.der")))).bind(PROFILE).bind(&attestation).execute(&f.pool).await.expect("trusted host fixture");
    }
    if let Ok(mut outsider) = client(tls, port, "worker").await {
        assert!(outsider.poll(announce.clone()).await.is_err());
    }
    let mut other_host = client(tls, port, "unadmitted")
        .await
        .expect("separate leaf on same CA");
    assert_eq!(
        other_host
            .poll(announce.clone())
            .await
            .expect_err("wrong leaf")
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut wrong = announce.clone();
    wrong.attestation_digest = hash_secret("unreviewed");
    assert_eq!(
        host.poll(wrong)
            .await
            .expect_err("wrong attestation")
            .code(),
        tonic::Code::PermissionDenied
    );
    assert!(
        host.poll(announce.clone())
            .await
            .expect("real host heartbeat")
            .into_inner()
            .assignment
            .is_none()
    );
    let absent = "/api/v1/agent/invocations/withdrawn-before-create";
    let withdrawn = request(&f, "DELETE", absent, None, keyed(&f, "withdraw")).await;
    assert_eq!(withdrawn.0, StatusCode::OK);
    assert!(withdrawn.1.get("run").is_none());
    assert_eq!(
        withdrawn,
        request(&f, "DELETE", absent, None, keyed(&f, "retry")).await
    );
    assert_eq!(
        request(
            &f,
            "POST",
            API,
            Some(body.clone()),
            keyed(&f, "withdrawn-before-create")
        )
        .await
        .0,
        StatusCode::GONE
    );
    let (a, b) = tokio::join!(
        request(&f, "POST", API, Some(body.clone()), keyed(&f, "same")),
        request(&f, "POST", API, Some(body.clone()), keyed(&f, "same"))
    );
    assert_eq!(a.0, StatusCode::CREATED, "{}", a.1);
    assert_eq!(a, b);
    let run = a.1["run_id"].as_str().expect("run");
    let endpoint = format!("{API}/{run}");
    assert_eq!(a.1["status"], "queued");
    assert_eq!(a.1["workspace"]["status"], "requested");
    let mut changed = body.clone();
    changed["workspace_ttl_seconds"] = json!(301);
    assert_eq!(
        request(&f, "POST", API, Some(changed), keyed(&f, "same"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&other, "GET", &endpoint, None, headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&other, "DELETE", &endpoint, None, headers(&other))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &f,
            "POST",
            API,
            Some(body.clone()),
            keyed(&f, "over-host-capacity")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let assignment = host
        .poll(announce.clone())
        .await
        .expect("assignment")
        .into_inner()
        .assignment
        .expect("queued work");
    assert_eq!(assignment.run_id, run);
    assert_eq!(assignment.workspace_id, a.1["workspace"]["workspace_id"]);
    let claim = host::ClaimWorkspaceRequest {
        host_instance_id: host_id.clone(),
        run_id: run.into(),
        workspace_id: assignment.workspace_id.clone(),
        expected_workspace_version: assignment.workspace_version,
        command_id: "claim-once".into(),
    };
    let lease = host
        .claim_workspace(claim.clone())
        .await
        .expect("claim")
        .into_inner();
    assert_eq!(
        lease,
        host.claim_workspace(claim.clone())
            .await
            .expect("same receipt")
            .into_inner()
    );
    assert_eq!(
        request(&f, "GET", &endpoint, None, headers(&f)).await.1["workspace"]["status"],
        "preparing"
    );
    let mut stale = lease.clone();
    stale.epoch += 1;
    assert!(host.renew_workspace(stale).await.is_err());
    assert!(host.renew_workspace(lease.clone()).await.is_ok());
    sqlx::query("SELECT pg_sleep(1.1)")
        .execute(&f.pool)
        .await
        .expect("advance real lease clock");
    host.poll(announce.clone()).await.expect("refresh liveness");
    let renewed = host
        .renew_workspace(lease.clone())
        .await
        .expect("renew after heartbeat")
        .into_inner();
    assert!(
        renewed.lease_until.as_ref().expect("new deadline").seconds
            > lease.lease_until.as_ref().expect("old deadline").seconds
    );
    assert_eq!(
        renewed,
        host.renew_workspace(lease.clone())
            .await
            .expect("lost renewal response replay")
            .into_inner()
    );
    let recovered = request(
        &f,
        "GET",
        "/api/v1/agent/invocations/same",
        None,
        headers(&f),
    )
    .await;
    assert_eq!(recovered.1["run_id"], run);
    assert_eq!(
        request(
            &other,
            "GET",
            "/api/v1/agent/invocations/same",
            None,
            headers(&other)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let cancelled = request(&f, "DELETE", &endpoint, None, keyed(&f, "cancel")).await;
    assert_eq!(cancelled.0, StatusCode::OK);
    assert_eq!(cancelled.1["status"], "cancelled");
    assert_eq!(cancelled.1["workspace"]["status"], "draining");
    assert!(host.renew_workspace(lease.clone()).await.is_err());
    assert!(host.claim_workspace(claim).await.is_err());
    assert_eq!(
        request(
            &f,
            "POST",
            API,
            Some(body.clone()),
            keyed(&f, "still-occupied")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let mut cleanup = host::WorkspaceCleanup {
        host_instance_id: host_id,
        run_id: run.into(),
        workspace_id: assignment.workspace_id,
        allocation_id: lease.allocation_id,
        allocation_epoch: lease.epoch,
        evidence_digest: hash_secret("explicit host cleanup fixture"),
        sandbox_absent: true,
        cgroup_absent: false,
    };
    assert!(host.confirm_cleanup(cleanup.clone()).await.is_err());
    assert!(
        sqlx::query(
            "UPDATE cloud_workspaces SET status='deleted',version=version+1 WHERE workspace_id=$1"
        )
        .bind(&cleanup.workspace_id)
        .execute(&f.pool)
        .await
        .is_err()
    );
    cleanup.cgroup_absent = true;
    assert!(other_host.confirm_cleanup(cleanup.clone()).await.is_err());
    let receipt = host
        .confirm_cleanup(cleanup.clone())
        .await
        .expect("host receipt")
        .into_inner();
    assert_eq!(
        receipt,
        host.confirm_cleanup(cleanup)
            .await
            .expect("idempotent cleanup")
            .into_inner()
    );
    assert_eq!(
        request(&f, "GET", &endpoint, None, headers(&f)).await.1["workspace"]["status"],
        "deleted"
    );
    let queued = request(&f, "POST", API, Some(body.clone()), keyed(&f, "second")).await;
    assert_eq!(queued.0, StatusCode::CREATED);
    sqlx::query("UPDATE cloud_workspace_quotas SET max_active=1 WHERE owner_id=$1")
        .bind(&f.user)
        .execute(&f.pool)
        .await
        .expect("owned quota fixture");
    assert_eq!(
        request(
            &f,
            "POST",
            API,
            Some(body.clone()),
            keyed(&f, "over-owner-capacity")
        )
        .await
        .1["code"],
        "WORKSPACE_QUOTA_EXCEEDED"
    );
    let second = queued.1["run_id"].as_str().expect("second");
    sqlx::query("UPDATE cloud_workspaces SET expires_at=clock_timestamp()-interval '1 second' WHERE run_id=$1").bind(second).execute(&f.pool).await.expect("expired reservation");
    let count = auth::routes::expire_cloud_workspaces(&f.pool)
        .await
        .expect("restart maintenance");
    assert_eq!(count, 1);
    assert_eq!(
        auth::routes::expire_cloud_workspaces(&f.pool)
            .await
            .expect("maintenance replay"),
        0
    );
    let head = request(&f, "GET", &format!("{API}?limit=1"), None, headers(&f)).await;
    assert_eq!(head.1["data"][0]["workspace"]["status"], "deleted");
    assert_eq!(head.1["data"][0]["status"], "cancelled");
    let next = format!("{API}?before={second}&limit=1");
    assert_eq!(
        request(&f, "GET", &next, None, headers(&f)).await.1["data"][0]["run_id"],
        run
    );
    assert_eq!(
        request(&other, "GET", &next, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let late = request(
        &f,
        "POST",
        API,
        Some(body.clone()),
        keyed(&f, "claimed-expiry"),
    )
    .await;
    assert_eq!(late.0, StatusCode::CREATED);
    let next = host
        .poll(announce.clone())
        .await
        .expect("claimed expiry")
        .into_inner()
        .assignment
        .expect("assignment");
    let third = host
        .claim_workspace(host::ClaimWorkspaceRequest {
            host_instance_id: announce.host_instance_id.clone(),
            run_id: next.run_id.clone(),
            workspace_id: next.workspace_id.clone(),
            expected_workspace_version: next.workspace_version,
            command_id: "third-claim".into(),
        })
        .await
        .expect("actual lease")
        .into_inner();
    sqlx::query("UPDATE cloud_workspaces SET lease_until=clock_timestamp()-interval '1 second' WHERE workspace_id=$1").bind(&next.workspace_id).execute(&f.pool).await.expect("expire only fixture lease");
    assert_eq!(
        auth::routes::expire_cloud_workspaces(&f.pool)
            .await
            .expect("expired lease maintenance"),
        1
    );
    let reclaim = host
        .poll(announce.clone())
        .await
        .expect("rediscover drained allocation")
        .into_inner()
        .reclaim
        .expect("original claim");
    assert_eq!(third.allocation_id, reclaim.allocation_id);
    host.confirm_cleanup(host::WorkspaceCleanup {
        host_instance_id: announce.host_instance_id.clone(),
        run_id: reclaim.run_id,
        workspace_id: reclaim.workspace_id,
        allocation_id: reclaim.allocation_id,
        allocation_epoch: reclaim.allocation_epoch,
        evidence_digest: hash_secret("expired fixture cleanup"),
        sandbox_absent: true,
        cgroup_absent: true,
    })
    .await
    .expect("cleanup after lease loss");
    let withdrawal = request(
        &f,
        "DELETE",
        "/api/v1/agent/invocations/claimed-expiry",
        None,
        keyed(&f, "withdraw-expired"),
    )
    .await;
    assert_eq!(withdrawal.0, StatusCode::OK);
    assert_eq!(withdrawal.1["run"]["status"], "cancelled");
    assert_eq!(withdrawal.1["run"]["workspace"]["status"], "deleted");
    assert_eq!(
        request(
            &f,
            "POST",
            API,
            Some(body.clone()),
            keyed(&f, "claimed-expiry")
        )
        .await
        .0,
        StatusCode::GONE
    );
    let mut draining = announce;
    draining.accept_reservations = false;
    host.poll(draining)
        .await
        .expect("Host stops accepting new work");
    assert_eq!(
        request(&f, "POST", API, Some(body), keyed(&f, "offline"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ledger_transactions")
            .fetch_one(&f.pool)
            .await
            .expect("isolated ledger"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT reserved FROM budget_roots WHERE root_budget_id=$1")
            .bind(budget["root_budget_id"].as_str().expect("budget id"))
            .fetch_one(&f.pool)
            .await
            .expect("budget unchanged"),
        0
    );
    if let Ok(driver) = std::env::var("ORANGE_CLOUD_WEB_DRIVER") {
        let parents = std::env::var("ORANGE_CLOUD_WEB_EVIDENCE").expect("owned evidence");
        browser_lifecycle(
            &f,
            &store,
            &mut host,
            host::HostPollRequest {
                host_instance_id: lease.host_instance_id,
                resource_profile_id: PROFILE.into(),
                attestation_digest: attestation,
                accept_reservations: true,
            },
            &driver,
            FsPath::new(&parents),
        )
        .await;
    }
    drop(host);
    drop(other_host);
    stop.send(()).expect("local stop");
    service.await.expect("service task").expect("clean stop");
}

async fn browser_lifecycle(
    f: &Fixture,
    store: &ArtifactStore,
    host: &mut PlatformHostPoolClient<Channel>,
    poll: host::HostPollRequest,
    driver: &str,
    evidence: &FsPath,
) {
    use std::{process::Stdio, time::Duration};
    use tokio::io::AsyncWriteExt;
    let app = f
        .router
        .clone()
        .merge(orange_control_plane::artifacts::http::router(
            f.identity.clone(),
            store.clone(),
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("real user API");
    let addr = listener.local_addr().expect("port");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    host.poll(poll.clone())
        .await
        .expect("live Host before browser");
    let mut child =
        tokio::process::Command::new(std::env::var("ORANGE_CLOUD_WEB_NODE").expect("Node"))
            .arg(driver)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("built Web driver");
    let payload = json!({"api":format!("http://{addr}"),"token":f.token,"user":f.user,"screenshot":evidence.join("cloud-cancelled.png")});
    let mut input = child.stdin.take().expect("private input");
    input
        .write_all(&serde_json::to_vec(&payload).expect("owned packet"))
        .await
        .expect("pass in memory");
    drop(input);
    let (finished, mut done) = tokio::sync::oneshot::channel();
    let browser = async {
        let result = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output()).await;
        let _ = finished.send(());
        result
    };
    let lifecycle = async {
        let mut cleaned = false;
        let mut claimed = None;
        loop {
            if !matches!(
                done.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ) {
                return cleaned;
            }
            let reply = host
                .poll(poll.clone())
                .await
                .expect("live Host poll")
                .into_inner();
            if !cleaned && let Some(next) = reply.assignment {
                assert!(claimed.is_none());
                let lease = host
                    .claim_workspace(host::ClaimWorkspaceRequest {
                        host_instance_id: poll.host_instance_id.clone(),
                        run_id: next.run_id,
                        workspace_id: next.workspace_id,
                        expected_workspace_version: next.workspace_version,
                        command_id: "ui-claim".into(),
                    })
                    .await
                    .expect("claim browser reservation")
                    .into_inner();
                claimed = Some(lease);
            }
            if let Some(reclaim) = reply.reclaim {
                let lease = claimed.as_ref().expect("original UI lease");
                assert_eq!(reclaim.allocation_id, lease.allocation_id);
                assert_eq!(reclaim.allocation_epoch, lease.epoch);
                assert!(host.renew_workspace(lease.clone()).await.is_err());
                host.confirm_cleanup(host::WorkspaceCleanup {
                    host_instance_id: poll.host_instance_id.clone(),
                    run_id: reclaim.run_id,
                    workspace_id: reclaim.workspace_id,
                    allocation_id: reclaim.allocation_id,
                    allocation_epoch: reclaim.allocation_epoch,
                    sandbox_absent: true,
                    cgroup_absent: true,
                    evidence_digest: hash_secret("explicit transport test cleanup receipt"),
                })
                .await
                .expect("confirm fixture cleanup");
                cleaned = true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    let (out, cleaned) = tokio::join!(browser, lifecycle);
    let out = out.expect("bounded browser").expect("driver exit");
    stop.send(()).expect("stop user API");
    service.await.expect("server task").expect("clean shutdown");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cleaned, "Browser workflow did not reach Host cleanup");
    let result: Value = serde_json::from_slice(&out.stdout).expect("browser receipt");
    let id = result["run_id"].as_str().expect("Run");
    let projection = request(f, "GET", &format!("{API}/{id}"), None, headers(f)).await;
    assert_eq!(projection.1["status"], "cancelled");
    assert_eq!(projection.1["workspace"]["status"], "deleted");
    assert_eq!(projection.1["input_version_id"], result["input_version_id"]);
    assert_eq!(result["creates"], 2);
    assert_eq!(result["uploads"], 2);
    assert_eq!(result["lookups"], 1);
    assert_eq!(result["cancellations"], 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM cloud_run_events WHERE run_id=$1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .expect("owned events"),
        4
    );
    let withdrawn:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_run_withdrawals f JOIN cloud_runs r USING(owner_id,user_id,request_key)
        JOIN cloud_workspaces w USING(run_id) WHERE f.receipt_id=$1 AND r.run_id=$2 AND r.status='cancelled' AND w.status='deleted' AND
        NOT EXISTS(SELECT 1 FROM cloud_workspace_claims a WHERE a.workspace_id=w.workspace_id))")
        .bind(result["withdrawal_receipt"].as_str().expect("withdrawal receipt")).bind(result["withdrawn_run"].as_str().expect("withdrawn run")).fetch_one(&f.pool).await.expect("independent queued withdrawal");
    assert!(withdrawn);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ledger_transactions")
            .fetch_one(&f.pool)
            .await
            .expect("no browser billing"),
        0
    );
    std::fs::write(
        evidence.join("browser-result.json"),
        serde_json::to_vec_pretty(&result).expect("report"),
    )
    .expect("evidence");
}
