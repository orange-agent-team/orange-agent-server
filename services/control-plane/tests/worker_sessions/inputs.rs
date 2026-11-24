//! Real customer input files and provider mTLS delivery. Prices, enrollment,
//! user sessions and OS/device probes are explicit fixtures; no model is run.
#[path = "inputs/cancel_cases.rs"]
mod cancel_cases;
#[path = "inputs/cleanup_cases.rs"]
mod cleanup_cases;
#[cfg(all(unix, feature = "real-worker-tests"))]
#[path = "inputs/cpu_cases.rs"]
mod cpu_cases;
#[path = "inputs/job_queries.rs"]
mod job_queries;
#[path = "inputs/output_cases.rs"]
mod output_cases;
#[cfg(feature = "queue-expiry-tests")]
#[path = "inputs/queue_expiry.rs"]
mod queue_expiry;
#[path = "inputs/renewal_cases.rs"]
mod renewal_cases;
#[path = "inputs/withdrawals.rs"]
mod withdrawals;
use super::*;
use orange_control_plane::{
    artifacts::{ArtifactActor, ArtifactReader, ArtifactStore},
    auth::{
        hash_secret, random_secret,
        users::{USER_COOKIE, UserIdentity, UserOidcConfig},
    },
};
use orange_protocol::{
    CreateBudget, InvocationPurpose, SubmitJob,
    artifacts::{BindJobInput, CreateUpload},
    model::{GenerateRequest, Message, Role},
    request_digest,
};
use orange_worker::{
    broker::*,
    control::ControlConnection,
    execution_journal::{ExecutionJournal, ExecutionRecord},
    node_journal::NodeJournal,
};
use prost::Message as ProstMessage;
use tower::ServiceExt;
struct InputFixture {
    d: Device,
    objects: ArtifactStore,
    root: PathBuf,
    actor: ArtifactActor,
    owner: String,
    user: String,
    job: String,
    upload: CreateUpload,
    version: String,
    profile: String,
    model_sha: String,
    execution_digest: String,
    request: GenerateRequest,
    headers: axum::http::HeaderMap,
    identity: UserIdentity,
}
async fn setup(cert: &str) -> InputFixture {
    setup_model(
        cert,
        &"b".repeat(64),
        &"c".repeat(64),
        "private-input-fixture ".repeat(2500),
        32,
    )
    .await
}
async fn setup_model(
    cert: &str,
    model_sha: &str,
    execution_digest: &str,
    prompt: String,
    max_output_tokens: u32,
) -> InputFixture {
    setup_model_scope(
        cert,
        model_sha,
        execution_digest,
        prompt,
        max_output_tokens,
        false,
    )
    .await
}
async fn setup_model_scope(
    cert: &str,
    model_sha: &str,
    execution_digest: &str,
    prompt: String,
    max_output_tokens: u32,
    organization: bool,
) -> InputFixture {
    setup_model_meter(
        cert,
        model_sha,
        execution_digest,
        prompt,
        max_output_tokens,
        organization,
        None,
    )
    .await
}
async fn setup_model_meter(
    cert: &str,
    model_sha: &str,
    execution_digest: &str,
    prompt: String,
    max_output_tokens: u32,
    organization: bool,
    meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
) -> InputFixture {
    setup_input_case(
        cert,
        model_sha,
        execution_digest,
        prompt,
        max_output_tokens,
        InputAdmission {
            organization,
            meter,
            deadline_seconds: 3600,
        },
    )
    .await
}
struct InputAdmission {
    organization: bool,
    meter: Option<orange_model_runtime::meter_service::TrustedMeter>,
    deadline_seconds: i64,
}
async fn setup_input_case(
    cert: &str,
    model_sha: &str,
    execution_digest: &str,
    prompt: String,
    max_output_tokens: u32,
    admission: InputAdmission,
) -> InputFixture {
    setup_input_for_device(
        device(cert).await,
        model_sha,
        execution_digest,
        prompt,
        max_output_tokens,
        admission,
    )
    .await
}
async fn setup_input_for_device(
    d: Device,
    model_sha: &str,
    execution_digest: &str,
    prompt: String,
    max_output_tokens: u32,
    admission: InputAdmission,
) -> InputFixture {
    let InputAdmission {
        organization,
        meter,
        deadline_seconds,
    } = admission;
    assert!((1..=3600).contains(&deadline_seconds));
    let pool = d.store.pool();
    let owner = Uuid::new_v4().to_string();
    let user = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let token = random_secret();
    let csrf = random_secret();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(pool)
        .await
        .expect("customer owner");
    let fixture_budget = if std::env::var_os("ORANGE_CLOUD_DIRECTORY_TOOLS").is_some()
        || std::env::var_os("ORANGE_MODEL_CONNECTOR_FIXTURE").is_some()
        || std::env::var_os("ORANGE_MODEL_REPOSITORY_FIXTURE").is_some()
    {
        5000_i64
    } else {
        1000
    };
    sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,$2)")
        .bind(&owner)
        .bind(fixture_budget)
        .execute(pool)
        .await
        .expect("development credits");
    let personal = if organization {
        let personal = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
            .bind(&personal)
            .execute(pool)
            .await
            .expect("personal owner fixture");
        sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
            .bind(&personal)
            .execute(pool)
            .await
            .expect("personal wallet");
        personal
    } else {
        owner.clone()
    };
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'input-customer-fixture',$1,$2)").bind(&user).bind(&personal).execute(pool).await.expect("customer user");
    if organization {
        sqlx::query("INSERT INTO organizations(organization_id,name) VALUES($1,'Output permission fixture')").bind(&owner).execute(pool).await.expect("organization");
        sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'owner')")
            .bind(&owner)
            .bind(&user)
            .execute(pool)
            .await
            .expect("membership");
    }
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,0,floor(extract(epoch FROM clock_timestamp()))::bigint+3600,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)").bind(hash_secret(&token)).bind(&session).bind(&user).bind(&csrf).execute(pool).await.expect("session fixture");
    let config: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity config"))
            .expect("config"),
    )
    .expect("parse");
    let origin = config.user_origin.clone();
    let identity = UserIdentity::discover(pool.clone(), config)
        .await
        .expect("real local discovery");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cookie",
        format!("{USER_COOKIE}={token}").parse().expect("cookie"),
    );
    headers.insert("origin", origin.parse().expect("origin"));
    headers.insert("x-csrf-token", csrf.parse().expect("csrf"));
    headers.insert("idempotency-key", "input-fixture".parse().expect("key"));
    let actor = identity
        .artifact_actor(&headers, true)
        .await
        .expect("actual resolver");
    let root = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("private root"))
        .join(format!("input-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).expect("dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("private");
    }
    let objects =
        ArtifactStore::open_local_with_meter(pool.clone(), &root, 1024 * 1024, 100, meter)
            .await
            .expect("real objects");
    let request = GenerateRequest {
        invocation_id: Uuid::new_v4().to_string(),
        model_version: "model-test".into(),
        messages: vec![Message::plain(Role::User, prompt)],
        tools: vec![],
        max_output_tokens,
    };
    let bytes = serde_json::to_vec(&request).expect("input");
    let upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    objects
        .begin_upload(&actor, &owner, &upload)
        .await
        .expect("upload intent");
    let v = objects
        .put(
            actor.clone(),
            owner.clone(),
            upload.upload_id.clone(),
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("real upload");
    let budget = d
        .store
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: Uuid::new_v4().to_string(),
                limit: fixture_budget.to_string().try_into().expect("money"),
            },
        )
        .await
        .expect("root");
    if organization {
        sqlx::query(
            "INSERT INTO budget_user_grants(root_budget_id,user_id,can_submit) VALUES($1,$2,true)",
        )
        .bind(&budget.root_budget_id)
        .bind(&user)
        .execute(pool)
        .await
        .expect("explicit budget grant fixture");
    }
    let quote = Uuid::new_v4().to_string();
    let profile = Uuid::new_v4().to_string();
    let projection = request_digest("model-request-v1", &request).expect("projection");
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'input-fixture',100,clock_timestamp()+interval '1 hour')").bind(&quote).bind(&owner).bind(&budget.root_budget_id).bind(&request.invocation_id).bind(&projection).execute(pool).await.expect("price/quote fixture");
    sqlx::query("WITH explicit_legacy_review AS (INSERT INTO model_legacy_profile_pins(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,'model-test',$2,$3) RETURNING execution_profile_id,model_version_id,model_sha256,execution_profile_digest) INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) SELECT execution_profile_id,model_version_id,model_sha256,execution_profile_digest FROM explicit_legacy_review").bind(&profile).bind(model_sha).bind(execution_digest).execute(pool).await.expect("reviewed fixture");
    sqlx::query("INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline) VALUES($1,$2,$3,$4,date_trunc('second',clock_timestamp()+make_interval(secs=>$5)))").bind(&quote).bind(&d.node).bind(&profile).bind(&projection).bind(deadline_seconds as f64).execute(pool).await.expect("scheduler fixture");
    let submit = SubmitJob {
        root_budget_id: budget.root_budget_id,
        invocation_id: request.invocation_id.clone(),
        purpose: InvocationPurpose::Primary,
        quote_id: quote,
        request_projection_digest: projection,
    };
    let app = orange_control_plane::artifacts::http::router(identity.clone(), objects.clone());
    let diagnostic_test = std::env::var_os("ORANGE_DIAGNOSTIC_CORRELATION_TEST").is_some();
    let app = orange_control_plane::diagnostics::observe(
        app,
        orange_control_plane::telemetry::ApiRole::User,
        diagnostic_test,
    );
    let mut http = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/owners/{owner}/jobs"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&submit).expect("body"),
        ))
        .expect("request");
    *http.headers_mut() = headers.clone();
    http.headers_mut()
        .insert("content-type", "application/json".parse().expect("type"));
    if diagnostic_test {
        http.headers_mut().insert(
            "x-orange-diagnostic-id",
            "must-not-enter-log".parse().expect("test header"),
        );
    }
    let response = app.oneshot(http).await.expect("actual job HTTP router");
    let diagnostic_id = response.headers()["x-orange-diagnostic-id"]
        .to_str()
        .expect("request reference")
        .to_owned();
    assert_eq!(response.status(), 200);
    let job: orange_protocol::JobView = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("response"),
    )
    .expect("job view");
    let job = job.job_id;
    if diagnostic_test {
        println!(
            "expected_job_event {} {}",
            diagnostic_id,
            orange_control_plane::diagnostics::link("job", &job)
        );
    }
    InputFixture {
        d,
        objects,
        root,
        actor,
        owner,
        user,
        job,
        upload,
        version: v.version_id,
        profile,
        model_sha: model_sha.into(),
        execution_digest: execution_digest.into(),
        request,
        headers,
        identity,
    }
}
async fn delete_input(f: &InputFixture) -> (axum::http::StatusCode, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut req = axum::http::Request::delete(format!(
        "/api/v1/owners/{}/uploads/{}",
        f.owner, f.upload.upload_id
    ))
    .body(axum::body::Body::empty())
    .expect("delete request");
    *req.headers_mut() = f.headers.clone();
    req.headers_mut().insert(
        "idempotency-key",
        f.upload.upload_id.parse().expect("original upload key"),
    );
    let response = app.oneshot(req).await.expect("actual delete route");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .expect("response bytes");
    (
        status,
        serde_json::from_slice(&body).expect("typed deletion response"),
    )
}
async fn bind(f: &InputFixture) {
    let req = BindJobInput {
        upload_id: f.upload.upload_id.clone(),
        version_id: f.version.clone(),
    };
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut http = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/owners/{}/jobs/{}/input", f.owner, f.job))
        .body(axum::body::Body::from(
            serde_json::to_vec(&req).expect("body"),
        ))
        .expect("request");
    *http.headers_mut() = f.headers.clone();
    http.headers_mut()
        .insert("content-type", "application/json".parse().expect("type"));
    http.headers_mut()
        .insert("idempotency-key", f.job.parse().expect("key"));
    let response = app.clone().oneshot(http).await.expect("bind HTTP");
    assert_eq!(response.status(), 200);
    let a: orange_protocol::artifacts::JobInputView = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("body"),
    )
    .expect("binding");
    assert_eq!(a.sha256, f.upload.sha256);
    assert_eq!(
        f.objects
            .bind_job_input(&f.actor, &f.owner, &f.job, &req)
            .await
            .expect("same original binding"),
        a
    );
    let mut get = axum::http::Request::builder()
        .uri(format!("/api/v1/owners/{}/jobs/{}", f.owner, f.job))
        .body(axum::body::Body::empty())
        .expect("status request");
    *get.headers_mut() = f.headers.clone();
    assert_eq!(
        app.oneshot(get).await.expect("current own Job").status(),
        200
    );
}
async fn receiver(
    f: &InputFixture,
    server: &Running,
) -> (
    ControlConnection,
    NodeJournal,
    ExecutionJournal,
    ExecutionRecord,
) {
    receiver_mode(f, server, false).await
}
async fn receiver_mode(
    f: &InputFixture,
    server: &Running,
    renewal: bool,
) -> (
    ControlConnection,
    NodeJournal,
    ExecutionJournal,
    ExecutionRecord,
) {
    receiver_capabilities(f, server, renewal, false).await
}
async fn receiver_capabilities(
    f: &InputFixture,
    server: &Running,
    renewal: bool,
    output: bool,
) -> (
    ControlConnection,
    NodeJournal,
    ExecutionJournal,
    ExecutionRecord,
) {
    let base = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("root"));
    let mut node = NodeJournal::open(
        &base.join(format!("node-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("node journal");
    let mut journal = ExecutionJournal::open(
        &base.join(format!("execution-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("journal");
    let config = worker_config(server, &f.d);
    let mut connection = if output {
        ControlConnection::connect_output_execution(config, &mut node, &mut journal).await
    } else if renewal {
        ControlConnection::connect_renewable_execution(config, &mut node, &mut journal).await
    } else {
        ControlConnection::connect_execution_with_inputs(config, &mut node, &mut journal).await
    }
    .expect("negotiated input");
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = connection
                .poll_execution("input-capabilities", &mut node, &mut journal)
                .await
                .expect("poll");
            if let Some(a) = p.assignment {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("assignment");
    (connection, node, journal, record)
}
async fn broker(f: &InputFixture) -> (Broker, Supervisor) {
    let mut broker = Broker::open(
        &f.root.join(format!("broker-{}.sqlite", Uuid::new_v4())),
        BrokerConfig {
            owner_identity: "input-provider".into(),
            devices: vec!["cpu".into()],
            approved_profiles: vec![ApprovedProfile {
                id: f.profile.clone(),
                model_sha256: f.model_sha.clone(),
                execution_digest: f.execution_digest.clone(),
            }],
        },
    )
    .expect("Broker");
    broker
        .bind_enrolled_node(&f.d.node)
        .expect("enroll fixture");
    broker
        .reconcile("cpu", &super::policy_cases::PolicyProbeFixture)
        .await
        .expect("explicit device probe");
    broker.set_remote_allowed(true).expect("consent");
    let supervisor = broker
        .register_supervisor(
            VerifiedPeer::from_verified_os_identity(
                "input-provider".into(),
                format!("fixture-process-{}", Uuid::new_v4()),
                Uuid::new_v4().to_string(),
                "n".repeat(32),
            )
            .expect("OS fixture"),
        )
        .expect("supervisor");
    (broker, supervisor)
}
async fn assignment(f: &InputFixture) -> worker::Assignment {
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT assignment FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("stored command");
    worker::Assignment::decode(bytes.as_slice()).expect("decode")
}
#[tokio::test]
async fn exact_customer_input_requires_start_and_rechecks_deletion_on_real_mtls() {
    let f = setup("client-ay").await;
    bind(&f).await;
    let reader = ArtifactReader::open_local(f.d.store.pool().clone(), &f.root)
        .await
        .expect("concurrent read-only root");
    let server = Running::start_with_input(&f.d.store, Some(reader.clone())).await;
    let (mut connection, mut node, mut journal, record) = receiver(&f, &server).await;
    let a = assignment(&f).await;
    if std::env::var_os("ORANGE_DIAGNOSTIC_CORRELATION_TEST").is_some() {
        let lease = a.lease.as_ref().expect("lease");
        use orange_control_plane::diagnostics::link;
        println!(
            "expected_assignment {} {} {}",
            link("job", &lease.job_id),
            link("attempt", &lease.attempt_id),
            link("node", &a.node_id)
        );
    }
    assert_eq!(a.input_artifacts.len(), 1);
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("mTLS");
    let read = worker::InputReadRequest {
        session_id: connection.identity().session_id.clone(),
        lease: a.lease.clone(),
        artifact_id: a.input_artifacts[0].artifact_id.clone(),
        version_id: f.version.clone(),
        offset: 0,
        max_bytes: 32768,
    };
    assert!(
        raw.read_input(read.clone()).await.is_err(),
        "no input before cloud start"
    );
    let (mut broker, supervisor) = broker(&f).await;
    connection
        .reserve_execution(
            &mut journal,
            &mut broker,
            &supervisor,
            "cpu",
            &record.command_id,
        )
        .expect("reserve");
    let grant = connection
        .authorize_execution(
            &mut journal,
            &mut node,
            "input-capabilities",
            &record.command_id,
        )
        .await
        .expect("start");
    let started = grant
        .confirm(&mut journal, &mut broker, &supervisor)
        .expect("confirm");
    let fetched = connection
        .fetch_model_input(&mut journal, &started, &record.command_id)
        .await
        .expect("actual multi-chunk bytes, no user cookie sent to provider");
    assert_eq!(
        serde_json::to_value(fetched).expect("value"),
        serde_json::to_value(&f.request).expect("expected")
    );
    let mut wrong = read.clone();
    wrong.version_id = Uuid::new_v4().to_string();
    assert!(raw.read_input(wrong).await.is_err());
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("customer deletion");
    assert!(
        raw.read_input(read).await.is_err(),
        "current deletion fences old lease"
    );
    connection.close();
    server.stop().await;
    reader.drain().await.expect("reader drain");
    f.objects.drain().await.expect("writer drain");
}
#[tokio::test]
async fn unbound_and_legacy_clients_cannot_receive_customer_input_assignments() {
    let f = setup("client-az").await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let mut client = server.client(Some(&f.d.certificate)).await.expect("client");
    let mut h = hello(&f.d.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.execution_control_version = 2;
    }
    let (sender, mut stream) = control(&mut client, h).await.expect("legacy execution");
    let w = welcome(&mut stream).await;
    heartbeat(&sender, &mut stream, &f.d.node, &w)
        .await
        .expect("no assignment before binding");
    bind(&f).await;
    heartbeat(&sender, &mut stream, &f.d.node, &w)
        .await
        .expect("no input for version0");
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_execution_attempts WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no attempt");
    assert_eq!(n, 0);
    f.d.store
        .cancel_queued(&f.owner, &f.job, "cancel-bound")
        .await
        .expect("queued cancel");
    let pins: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact_pins WHERE reference_id=$1")
        .bind(format!("job-input:{}", f.job))
        .fetch_one(f.d.store.pool())
        .await
        .expect("pins");
    assert_eq!(pins, 0);
    assert!(
        f.objects
            .bind_job_input(
                &f.actor,
                &f.owner,
                &f.job,
                &BindJobInput {
                    upload_id: f.upload.upload_id.clone(),
                    version_id: f.version.clone()
                }
            )
            .await
            .is_err()
    );
    drop(sender);
    drop(stream);
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn current_requester_revocation_and_superseded_epoch_stop_input_delivery() {
    let f = setup("client-ba").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (mut connection, mut node, mut journal, record) = receiver(&f, &server).await;
    let (mut broker, supervisor) = broker(&f).await;
    connection
        .reserve_execution(
            &mut journal,
            &mut broker,
            &supervisor,
            "cpu",
            &record.command_id,
        )
        .expect("reserve");
    let started = connection
        .authorize_execution(
            &mut journal,
            &mut node,
            "input-capabilities",
            &record.command_id,
        )
        .await
        .expect("grant")
        .confirm(&mut journal, &mut broker, &supervisor)
        .expect("confirm");
    sqlx::query("UPDATE users SET active=false WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("requester revocation fixture");
    assert!(
        connection
            .fetch_model_input(&mut journal, &started, &record.command_id)
            .await
            .is_err()
    );
    let a = assignment(&f).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("raw");
    let old = worker::InputReadRequest {
        session_id: a.session_id,
        lease: a.lease,
        artifact_id: a.input_artifacts[0].artifact_id.clone(),
        version_id: f.version.clone(),
        offset: 0,
        max_bytes: 1,
    };
    let (sender, mut stream) = control(&mut raw, hello(&f.d.node))
        .await
        .expect("superseding epoch");
    welcome(&mut stream).await;
    assert!(raw.read_input(old).await.is_err());
    drop(sender);
    drop(stream);
    connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn changed_projection_and_other_provider_cannot_inherit_input_authority() {
    let f = setup("client-bb").await;
    let mut changed = f.request.clone();
    changed.messages[0].content = "different request".into();
    let bytes = serde_json::to_vec(&changed).expect("bytes");
    let wrong = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    f.objects
        .begin_upload(&f.actor, &f.owner, &wrong)
        .await
        .expect("other valid file");
    let v = f
        .objects
        .put(
            f.actor.clone(),
            f.owner.clone(),
            wrong.upload_id.clone(),
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("publish");
    assert!(
        f.objects
            .bind_job_input(
                &f.actor,
                &f.owner,
                &f.job,
                &BindJobInput {
                    upload_id: wrong.upload_id,
                    version_id: v.version_id
                }
            )
            .await
            .is_err()
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM job_inputs WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("unbound");
    assert_eq!(n, 0);
    bind(&f).await;
    assert!(
        sqlx::query("UPDATE jobs SET submitting_user_id=$2 WHERE job_id=$1")
            .bind(&f.job)
            .bind(&f.d.user)
            .execute(f.d.store.pool())
            .await
            .is_err()
    );
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let (mut connection, mut node, mut journal, record) = receiver(&f, &server).await;
    let (mut broker, supervisor) = broker(&f).await;
    connection
        .reserve_execution(
            &mut journal,
            &mut broker,
            &supervisor,
            "cpu",
            &record.command_id,
        )
        .expect("reserve");
    let started = connection
        .authorize_execution(
            &mut journal,
            &mut node,
            "input-capabilities",
            &record.command_id,
        )
        .await
        .expect("grant")
        .confirm(&mut journal, &mut broker, &supervisor)
        .expect("confirm");
    // Closing the browser session is not cancellation of an already authorized
    // bounded Job. Account/ACL revocation, tested separately, still stops reads.
    sqlx::query("UPDATE user_sessions SET revoked=TRUE WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("browser logout fixture");
    connection
        .fetch_model_input(&mut journal, &started, &record.command_id)
        .await
        .expect("bounded Job authority independent of browser cookie");
    let a = assignment(&f).await;
    let other = device("client-bc").await;
    let mut client = server
        .client(Some(&other.certificate))
        .await
        .expect("other provider");
    let mut h = hello(&other.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut h.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.input_artifacts_version = 1;
    }
    let (sender, mut stream) = control(&mut client, h)
        .await
        .expect("other current session");
    let w = welcome(&mut stream).await;
    let request = worker::InputReadRequest {
        session_id: w.session_id,
        lease: a.lease,
        artifact_id: a.input_artifacts[0].artifact_id.clone(),
        version_id: f.version.clone(),
        offset: 0,
        max_bytes: 1,
    };
    assert!(client.read_input(request).await.is_err());
    drop(sender);
    drop(stream);
    connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[path = "inputs/job_history.rs"]
mod history_cases;

#[path = "inputs/job_socket.rs"]
mod socket_cases;
