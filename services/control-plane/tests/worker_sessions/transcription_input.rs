use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use orange_control_plane::{
    artifacts::{ArtifactStore, AudioMeter, http},
    auth::users::{UserIdentity, UserOidcConfig},
};
use orange_protocol::{CreateBudget, InvocationPurpose, SubmitJob};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

struct Buyer {
    owner: String,
    user: String,
    token: String,
    csrf: String,
    origin: String,
}
impl Buyer {
    async fn create(db: &Store, origin: &str) -> Self {
        let owner = Uuid::new_v4().to_string();
        let user = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().simple().to_string();
        let csrf = Uuid::new_v4().simple().to_string();
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
            .bind(&owner)
            .execute(db.pool())
            .await
            .expect("buyer");
        sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,1000)")
            .bind(&owner)
            .execute(db.pool())
            .await
            .expect("fixture wallet");
        sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'input-consent',$1,$2)").bind(&user).bind(&owner).execute(db.pool()).await.expect("buyer identity");
        sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,0,
            floor(extract(epoch FROM clock_timestamp()))::bigint+600,floor(extract(epoch FROM clock_timestamp()))::bigint+600)")
            .bind(hex::encode(Sha256::digest(token.as_bytes()))).bind(Uuid::new_v4().to_string()).bind(&user).bind(&csrf).execute(db.pool()).await.expect("fixture session");
        Self {
            owner,
            user,
            token,
            csrf,
            origin: origin.to_owned(),
        }
    }
    async fn call(
        &self,
        app: &Router,
        method: &str,
        path: &str,
        key: &str,
        body: Vec<u8>,
        status: u16,
    ) -> Value {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("Cookie", format!("__Host-orange-user={}", self.token))
            .header("Origin", &self.origin)
            .header("X-CSRF-Token", &self.csrf)
            .header("Idempotency-Key", key)
            .header(
                "Content-Type",
                if method == "PUT" {
                    "application/octet-stream"
                } else {
                    "application/json"
                },
            )
            .body(Body::from(body))
            .expect("request");
        let res = app.clone().oneshot(req).await.expect("router");
        let code = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), 65536).await.expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(code, status, "{path}: {value}");
        value
    }
}

#[tokio::test]
async fn consent_and_confirmed_lease_gate_every_real_audio_chunk() {
    let config = PathBuf::from(std::env::var("ORANGE_ASR_PROBE_FILE").expect("probe config"));
    let (_send, stop) = tokio::sync::watch::channel(false);
    let fact = orange_worker::host::probe_transcription(&config, stop)
        .await
        .expect("actual approved CPU profile");
    let d = device("client-s").await;
    let db = &d.store;
    let root = tempfile::tempdir().expect("private root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    let store = ArtifactStore::open_local(db.pool().clone(), root.path(), 10485760, 50)
        .await
        .expect("object store");
    let cfg: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("config"))
            .expect("OIDC"),
    )
    .expect("config");
    let buyer = Buyer::create(db, &cfg.user_origin).await;
    let stranger = Buyer::create(db, &cfg.user_origin).await;
    let identity = UserIdentity::discover(db.pool().clone(), cfg)
        .await
        .expect("actual OIDC discovery");
    let users = orange_control_plane::auth::routes::router(identity.clone());
    let app = http::build_router(
        identity,
        store.clone(),
        Some(AudioMeter::from_file(&config).expect("meter")),
    )
    .merge(users);
    let reader = store.reader();
    let server = Running::start_with_input(db, Some(reader.clone())).await;
    let mut raw = server
        .client(Some(&d.certificate))
        .await
        .expect("device TLS");
    use orange_worker::{
        broker::{ApprovedProfile, Broker, BrokerConfig, VerifiedPeer},
        control::{ControlConnection, TranscriptionDownload},
        execution_journal::ExecutionJournal,
        node_journal::NodeJournal,
    };
    let private = tempfile::tempdir().expect("worker state");
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private journals");
    let mut node = NodeJournal::open(&private.path().join("node.sqlite"), d.node.clone())
        .expect("node journal");
    let mut journal =
        ExecutionJournal::open(&private.path().join("execution.sqlite"), d.node.clone())
            .expect("execution journal");
    let mut conn = ControlConnection::connect_transcription_inputs(
        worker_config(&server, &d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("negotiated lease-bound input");
    let w = conn.identity().clone();
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,$2,$3,$4)")
      .bind(&profile).bind(fact.profile.model_version).bind(&fact.profile.model_sha256).bind(&fact.execution_profile_digest).execute(db.pool()).await.expect("independent review fixture");
    let observation = conn
        .report_transcription_profile(&profile, &fact)
        .await
        .expect("actual observation");
    let raw_audio =
        std::fs::read(std::env::var("ORANGE_AUDIO_SAMPLE").expect("sample")).expect("audio");
    let base = format!("/api/v1/owners/{}", buyer.owner);
    let upload = "audio-fixture";
    let endpoint = format!("{base}/uploads/{upload}");
    buyer.call(&app,"POST",&format!("{base}/uploads"),upload,serde_json::to_vec(&json!({"upload_id":upload,"sha256":fact.sample_sha256,"size_bytes":raw_audio.len().to_string(),"media_type":"audio/wav"})).expect("json"),200).await;
    let available = buyer
        .call(
            &app,
            "PUT",
            &format!("{endpoint}/content"),
            upload,
            raw_audio.clone(),
            200,
        )
        .await;
    let measurement = buyer
        .call(
            &app,
            "POST",
            &format!("{endpoint}/transcription-measurements"),
            upload,
            serde_json::to_vec(&json!({"version_id":available["version_id"]})).expect("json"),
            200,
        )
        .await;
    let budget = db
        .create_budget(
            &buyer.owner,
            &Uuid::new_v4().to_string(),
            &CreateBudget {
                scope_id: Uuid::new_v4().to_string(),
                limit: "1000".to_owned().try_into().expect("budget"),
            },
        )
        .await
        .expect("budget");
    let grant = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let path = format!("{base}/transcription-input-grants");
    let target = buyer
        .call(
            &app,
            "GET",
            &format!(
                "/api/v1/transcription-targets/{}",
                observation.observation_id
            ),
            "",
            vec![],
            200,
        )
        .await;
    assert_eq!(target["node_id"], d.node);
    assert_eq!(
        target["execution_profile_digest"],
        fact.execution_profile_digest
    );
    let request = json!({"grant_id":grant,"root_budget_id":budget.root_budget_id,"invocation_id":invocation,"upload_id":upload,
        "observation_id":observation.observation_id,"node_id":target["node_id"],"region_id":target["region_id"],"execution_profile_digest":target["execution_profile_digest"],"language":"en","ttl_seconds":600,"allow_remote_transcription":true,"measurement":measurement});
    let mut v = request.clone();
    v["allow_remote_transcription"] = json!(false);
    buyer
        .call(&app, "POST", &path, &grant, v.to_string().into_bytes(), 422)
        .await;
    let mut v = request.clone();
    v["measurement"]["duration_ms"] = json!("1");
    buyer
        .call(&app, "POST", &path, &grant, v.to_string().into_bytes(), 409)
        .await;
    stranger
        .call(
            &app,
            "POST",
            &path,
            &grant,
            request.to_string().into_bytes(),
            403,
        )
        .await;
    let mut changed_target = request.clone();
    changed_target["region_id"] = json!("different-region");
    buyer
        .call(
            &app,
            "POST",
            &path,
            &grant,
            changed_target.to_string().into_bytes(),
            409,
        )
        .await;
    let early = Uuid::new_v4().to_string();
    let withdrawn = buyer
        .call(
            &app,
            "DELETE",
            &format!("{path}/{early}"),
            &early,
            vec![],
            200,
        )
        .await;
    assert_eq!(withdrawn["state"], "revoked");
    let mut overtaken = request.clone();
    overtaken["grant_id"] = json!(early);
    overtaken["invocation_id"] = json!(Uuid::new_v4().to_string());
    buyer
        .call(
            &app,
            "POST",
            &path,
            &early,
            overtaken.to_string().into_bytes(),
            409,
        )
        .await;
    let consent = buyer
        .call(
            &app,
            "POST",
            &path,
            &grant,
            request.to_string().into_bytes(),
            200,
        )
        .await;
    assert_eq!(consent["state"], "active");
    let status_path = format!("{path}/{grant}");
    assert_eq!(
        buyer.call(&app, "GET", &status_path, "", vec![], 200).await,
        consent
    );
    let projection = consent["projection_digest"].as_str().expect("projection");
    assert_eq!(
        buyer
            .call(
                &app,
                "POST",
                &path,
                &grant,
                request.to_string().into_bytes(),
                200
            )
            .await,
        consent
    );
    let mut other = request.clone();
    other["language"] = json!("zh");
    buyer
        .call(
            &app,
            "POST",
            &path,
            &grant,
            other.to_string().into_bytes(),
            409,
        )
        .await;
    if let Ok(driver) = std::env::var("ORANGE_AUDIO_CONSENT_BROWSER") {
        use tokio::io::AsyncWriteExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("owned API");
        let addr = listener.local_addr().expect("port");
        let (shutdown, done) = oneshot::channel();
        let router = app.clone();
        let api = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = done.await;
                })
                .await
                .expect("API");
        });
        let fixture = json!({"origin":buyer.origin,"api":format!("http://{addr}"),"token":buyer.token,"upload":upload,"node":d.node,
            "observation":observation.observation_id,"budget":budget.root_budget_id,"region":"local-test",
            "screenshot":std::env::var("ORANGE_AUDIO_CONSENT_SCREENSHOT").expect("output")});
        let mut browser =
            tokio::process::Command::new(std::env::var("ORANGE_AUDIO_CONSENT_NODE").expect("Node"))
                .arg(driver)
                .stdin(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("actual Chrome driver");
        let mut stdin = browser.stdin.take().expect("private fixture pipe");
        stdin
            .write_all(&serde_json::to_vec(&fixture).expect("fixture"))
            .await
            .expect("send fixture");
        drop(stdin);
        let status=tokio::time::timeout(Duration::from_secs(90),async{
            loop{tokio::select!{r=browser.wait()=>break r.expect("driver exit"),_=tokio::time::sleep(Duration::from_secs(3))=>{
                conn.poll_execution("audio-input-test",&mut node,&mut journal).await.expect("maintain observed peer during UI selection");
            }}}
        }).await.expect("browser deadline");
        let _ = shutdown.send(());
        api.await.expect("API stopped");
        assert!(status.success(), "browser flow");
    }
    let no_jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(db.pool())
        .await
        .expect("jobs");
    assert_eq!(no_jobs, 0);
    // Separate trusted scheduler/admission fixture. The transport test exercises the real offer/ACK barrier.
    let quote = Uuid::new_v4().to_string();
    let job = Uuid::new_v4().to_string();
    let submitted = SubmitJob {
        root_budget_id: budget.root_budget_id.clone(),
        invocation_id: invocation.clone(),
        purpose: InvocationPurpose::Transcription,
        quote_id: quote.clone(),
        request_projection_digest: projection.to_owned(),
    };
    let mut tx = db.pool().begin().await.expect("queue fixture");
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
      VALUES($1,$2,$3,$4,$5,'input-transport-fixture',0,clock_timestamp()+interval '10 minutes')")
      .bind(&quote).bind(&buyer.owner).bind(&budget.root_budget_id).bind(&invocation).bind(projection).execute(&mut *tx).await.expect("quote fixture");
    sqlx::query("INSERT INTO quote_execution_plans(quote_id,target_node_id,execution_profile_id,request_projection_digest,job_deadline)
      VALUES($1,$2,$3,$4,clock_timestamp()+interval '10 minutes')").bind(&quote).bind(&d.node).bind(&profile).bind(projection).execute(&mut *tx).await.expect("plan fixture");
    sqlx::query("INSERT INTO jobs(job_id,owner_id,root_budget_id,invocation_id,purpose,quote_id,request_digest,execution_state,reserved,held_gift,held_earned,held_purchased,submitting_user_id,input_required)
      VALUES($1,$2,$3,$4,'transcription',$5,$6,'queued',0,0,0,0,$7,true)").bind(&job).bind(&buyer.owner).bind(&budget.root_budget_id).bind(&invocation)
      .bind(&quote).bind(orange_protocol::request_digest("submit_job",&submitted).expect("digest")).bind(&buyer.user).execute(&mut *tx).await.expect("queued job fixture");
    tx.commit().await.expect("queue");
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let poll = conn
                .poll_execution("audio-input-test", &mut node, &mut journal)
                .await
                .expect("actual control poll");
            if let Some(record) = poll.assignment {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real assignment");
    use prost::Message;
    let assignment_bytes: Vec<u8> =
        sqlx::query_scalar("SELECT assignment FROM worker_execution_attempts WHERE command_id=$1")
            .bind(&record.command_id)
            .fetch_one(db.pool())
            .await
            .expect("persisted assignment");
    let assignment =
        worker::Assignment::decode(assignment_bytes.as_slice()).expect("wire assignment");
    assert_eq!(assignment.lease.as_ref().expect("lease").job_id, job);
    let read = worker::TranscriptionInputRead {
        version: 1,
        session_id: w.session_id.clone(),
        lease: assignment.lease.clone(),
        grant_id: grant.clone(),
        projection_digest: projection.into(),
        offset: 0,
    };
    assert!(
        raw.read_transcription_input(read.clone()).await.is_err(),
        "unstarted lease"
    );
    let mut broker = Broker::open(
        &private.path().join("broker.sqlite"),
        BrokerConfig {
            owner_identity: "audio-provider".into(),
            devices: vec!["cpu".into()],
            approved_profiles: vec![ApprovedProfile {
                id: profile.clone(),
                model_sha256: fact.profile.model_sha256.clone(),
                execution_digest: fact.execution_profile_digest.clone(),
            }],
        },
    )
    .expect("local Broker");
    broker
        .bind_enrolled_node(&d.node)
        .expect("fixture enrollment");
    broker
        .reconcile("cpu", &super::policy_cases::PolicyProbeFixture)
        .await
        .expect("device probe fixture");
    broker.set_remote_allowed(true).expect("provider opt-in");
    let supervisor = broker
        .register_supervisor(
            VerifiedPeer::from_verified_os_identity(
                "audio-provider".into(),
                format!("test-{}", Uuid::new_v4()),
                Uuid::new_v4().to_string(),
                "n".repeat(32),
            )
            .expect("OS peer fixture"),
        )
        .expect("supervisor");
    conn.reserve_execution(
        &mut journal,
        &mut broker,
        &supervisor,
        "cpu",
        &record.command_id,
    )
    .expect("real reservation");
    let cloud = conn
        .authorize_execution(
            &mut journal,
            &mut node,
            "audio-input-test",
            &record.command_id,
        )
        .await
        .expect("real start grant");
    let started = cloud
        .confirm(&mut journal, &mut broker, &supervisor)
        .expect("confirmed durable start");
    let reference = assignment
        .transcription_input
        .as_ref()
        .expect("negotiated audio reference");
    assert_eq!(
        (&reference.grant_id, &reference.observation_id),
        (&grant, &observation.observation_id)
    );
    let text_read = worker::InputReadRequest {
        session_id: w.session_id.clone(),
        lease: assignment.lease.clone(),
        artifact_id: measurement["artifact_id"]
            .as_str()
            .expect("artifact")
            .into(),
        version_id: measurement["version_id"].as_str().expect("version").into(),
        offset: 0,
        max_bytes: 1,
    };
    assert!(
        raw.read_input(text_read).await.is_err(),
        "legacy text RPC cannot deliver audio"
    );
    let mut stale = read.clone();
    stale.lease.as_mut().expect("lease").fencing_token += 1;
    assert!(
        raw.read_transcription_input(stale).await.is_err(),
        "wrong fence"
    );
    let mut foreign = server
        .client(Some("client-t"))
        .await
        .expect("foreign peer TLS");
    assert!(
        foreign
            .read_transcription_input(read.clone())
            .await
            .is_err()
    );
    let mut transfer = TranscriptionDownload::for_record(&record).expect("transfer");
    while !conn
        .read_transcription_chunk(&journal, &started, &record.command_id, &mut transfer)
        .await
        .expect("verified mTLS range")
    {}
    let received = transfer.finish().expect("exact complete input");
    assert_eq!(received.audio, raw_audio);
    assert_eq!(received.language.as_deref(), Some("en"));
    assert_eq!(received.accepted.measure.duration_ms, 11000);
    let cfg: Value =
        serde_json::from_slice(&std::fs::read(&config).expect("probe config")).expect("paths");
    let path_of = |k: &str| std::path::Path::new(cfg[k].as_str().expect("path"));
    let model = orange_model_runtime::transcription::Client::new(
        path_of("python"),
        path_of("model_cache"),
        path_of("private_slot"),
    )
    .expect("local backend");
    let (_cancel, watch) = tokio::sync::watch::channel(false);
    let candidate = tokio::time::timeout_at(
        tokio::time::Instant::from_std(started.stop_at()),
        model.transcribe(
            &received.audio,
            &received.accepted,
            received.language.as_deref(),
            watch,
        ),
    )
    .await
    .expect("within local lease")
    .expect("real provider-side inference");
    assert_eq!(candidate.measure, received.accepted.measure);
    let transcript = candidate.transcript.expect("complete candidate");
    assert_eq!(transcript.language, "en");
    assert_eq!(
        hex::encode(Sha256::digest(transcript.text.as_bytes())),
        fact.transcript_sha256
    );
    let step = orange_protocol::pricing::AUDIO_INPUT_CHUNK_BYTES;
    let mut next = read.clone();
    next.offset = step as u64;
    let key:String=sqlx::query_scalar("SELECT t.published_key FROM artifact_transfers t JOIN artifacts a ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1")
        .bind(available["artifact_id"].as_str().expect("artifact")).fetch_one(db.pool()).await.expect("fixture object key");
    let file = root.path().join(key);
    let mode = std::fs::metadata(&file).expect("object").permissions();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .expect("test-only corruption");
    let mut damaged = raw_audio.clone();
    damaged[step] ^= 1;
    std::fs::write(&file, &damaged).expect("changed real bytes");
    std::fs::set_permissions(&file, mode.clone()).expect("reseal fixture");
    assert_eq!(
        raw.read_transcription_input(next.clone())
            .await
            .expect_err("corrupt range")
            .code(),
        tonic::Code::DataLoss
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .expect("fixture repair");
    std::fs::write(&file, &raw_audio).expect("restore the approved bytes");
    std::fs::set_permissions(&file, mode).expect("reseal");
    let (arrived, resume) = reader.pause_after_verified_audio_chunk(next.offset).await;
    let mut peer = raw.clone();
    let inflight = tokio::spawn(async move { peer.read_transcription_input(next).await });
    tokio::time::timeout(Duration::from_secs(3), arrived)
        .await
        .expect("verified data has been read")
        .expect("gate");
    let revoke = format!("{path}/{grant}");
    stranger
        .call(&app, "DELETE", &revoke, &grant, vec![], 404)
        .await;
    let stopped = buyer
        .call(&app, "DELETE", &revoke, &grant, vec![], 200)
        .await;
    resume.send(()).expect("release I/O boundary");
    assert_eq!(
        inflight
            .await
            .expect("request joined")
            .expect_err("revoked after physical read")
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(stopped["state"], "revoked");
    assert_eq!(
        buyer
            .call(&app, "DELETE", &revoke, &grant, vec![], 200)
            .await,
        stopped
    );
    let replay = buyer
        .call(
            &app,
            "POST",
            &path,
            &grant,
            request.to_string().into_bytes(),
            200,
        )
        .await;
    assert_eq!(replay["state"], "revoked");
    assert_eq!(replay["projection_digest"], consent["projection_digest"]);
    assert!(
        raw.read_transcription_input(read).await.is_err(),
        "withdrawal fences remaining bytes"
    );
    assert_eq!(
        buyer.call(&app, "GET", &status_path, "", vec![], 200).await["state"],
        "revoked"
    );
    buyer
        .call(&app, "DELETE", &endpoint, upload, vec![], 200)
        .await;
    assert_eq!(
        buyer
            .call(&app, "DELETE", &revoke, &grant, vec![], 200)
            .await,
        stopped
    );
    let revokes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transcription_input_revocations WHERE grant_id IN ($1,$2)",
    )
    .bind(&grant)
    .bind(&early)
    .fetch_one(db.pool())
    .await
    .expect("one revocation");
    assert_eq!(revokes, 2);
    drop(conn);
    drop(raw);
    drop(foreign);
    server.stop().await;
    store.drain().await.expect("close");
}

#[tokio::test]
async fn live_legacy_input_session_survives_upgrade_and_audio_negotiation_cannot_be_inherited() {
    use orange_worker::{
        control::ControlConnection, execution_journal::ExecutionJournal, node_journal::NodeJournal,
    };
    let url = std::env::var("ORANGE_AUDIO_OLD_DATABASE_URL").expect("disposable historic database");
    assert!(
        url.rsplit('/')
            .next()
            .is_some_and(|s| s.starts_with("orange_audio_old_"))
    );
    let db = Store::connect(&url).await.expect("actual database");
    let history = sqlx::migrate!();
    sqlx::migrate::Migrator::with_migrations(
        history
            .iter()
            .filter(|m| m.version <= 51)
            .cloned()
            .collect(),
    )
    .run(db.pool())
    .await
    .expect("historical schema");
    let d = device_in(db.clone(), "client-u").await;
    let root = tempfile::tempdir().expect("artifact root");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).expect("private");
    let store = ArtifactStore::open_local(db.pool().clone(), root.path(), 1048576, 100)
        .await
        .expect("writer");
    let server = Running::start_with_input(&db, Some(store.reader())).await;
    let tmp = tempfile::tempdir().expect("journals");
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private state");
    let mut node =
        NodeJournal::open(&tmp.path().join("node.sqlite"), d.node.clone()).expect("node");
    let mut journal =
        ExecutionJournal::open(&tmp.path().join("exec.sqlite"), d.node.clone()).expect("execution");
    let mut old = ControlConnection::connect_execution_with_inputs(
        worker_config(&server, &d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("old input protocol on schema 51");
    assert!(
        old.poll_execution("old-capabilities", &mut node, &mut journal)
            .await
            .expect("old dispatch")
            .assignment
            .is_none()
    );
    db.migrate().await.expect("upgrade while connected");
    db.migrate().await.expect("repeat safely");
    assert!(
        old.poll_execution("old-capabilities", &mut node, &mut journal)
            .await
            .expect("old stream remains live")
            .assignment
            .is_none()
    );
    let version = || async {
        sqlx::query_scalar::<_, i32>(
            "SELECT transcription_input_version FROM worker_sessions WHERE node_id=$1",
        )
        .bind(&d.node)
        .fetch_one(db.pool())
        .await
        .expect("capability")
    };
    assert_eq!(version().await, 0);
    let mut audio = ControlConnection::connect_transcription_inputs(
        worker_config(&server, &d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("real capability opt in");
    assert!(audio.identity().connection_epoch > old.identity().connection_epoch);
    assert_eq!(version().await, 1);
    let mut legacy = ControlConnection::connect_execution_with_inputs(
        worker_config(&server, &d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("legacy reconnect");
    assert_eq!(
        version().await,
        0,
        "old peer must not inherit previous input version"
    );
    assert!(
        legacy
            .poll_execution("legacy", &mut node, &mut journal)
            .await
            .expect("normal legacy poll")
            .assignment
            .is_none()
    );
    assert!(
        audio
            .poll_execution("old-audio", &mut node, &mut journal)
            .await
            .is_err(),
        "superseded connection denied"
    );
    drop(old);
    drop(audio);
    drop(legacy);
    server.stop().await;
    store.drain().await.expect("close");
}
