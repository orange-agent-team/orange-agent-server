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
    let app = http::build_router(
        identity,
        store.clone(),
        Some(AudioMeter::from_file(&config).expect("meter")),
    );
    let reader = store.reader();
    let server = Running::start_with_input(db, Some(reader.clone())).await;
    let mut raw = server
        .client(Some(&d.certificate))
        .await
        .expect("device TLS");
    let mut h = execution_cases::execution_hello(&d.node, 2);
    if let Some(worker_frame::Body::Hello(body)) = &mut h.body {
        body.node_control_version = 2;
        body.rejection_receipts_version = 1;
        body.input_artifacts_version = 1;
    }
    let (sender, mut stream) = control(&mut raw, h).await.expect("control");
    let w = welcome(&mut stream).await;
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,$2,$3,$4)")
      .bind(&profile).bind(fact.profile.model_version).bind(&fact.profile.model_sha256).bind(&fact.execution_profile_digest).execute(db.pool()).await.expect("independent review fixture");
    let observation = raw
        .report_transcription_observation(worker::TranscriptionObservation {
            observation_version: 1,
            session_id: w.session_id.clone(),
            request_id: Uuid::new_v4().to_string(),
            execution_profile_id: profile.clone(),
            execution_profile_digest: fact.execution_profile_digest.clone(),
            model_version: fact.profile.model_version.into(),
            model_sha256: fact.profile.model_sha256.clone(),
            sample_sha256: fact.sample_sha256.clone(),
            duration_ms: fact.sample_duration_ms,
            normalized_samples: fact.normalized_samples,
            transcript_sha256: fact.transcript_sha256.clone(),
            compute_type: fact.profile.compute_type.clone(),
            profile_descriptor_json: serde_json::to_vec(&fact.profile).expect("descriptor"),
        })
        .await
        .expect("observation")
        .into_inner();
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
    sqlx::query("INSERT INTO jobs(job_id,owner_id,root_budget_id,invocation_id,purpose,quote_id,request_digest,execution_state,reserved,held_gift,held_earned,held_purchased,submitting_user_id)
      VALUES($1,$2,$3,$4,'transcription',$5,$6,'queued',0,0,0,0,$7)").bind(&job).bind(&buyer.owner).bind(&budget.root_budget_id).bind(&invocation)
      .bind(&quote).bind(orange_protocol::request_digest("submit_job",&submitted).expect("digest")).bind(&buyer.user).execute(&mut *tx).await.expect("queued job fixture");
    tx.commit().await.expect("queue");
    heartbeat(&sender, &mut stream, &d.node, &w)
        .await
        .expect("poll");
    let assignment = execution_cases::assignment(&mut stream).await;
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
    execution_cases::send(&sender, &execution_cases::ack(&assignment)).await;
    execution_cases::start(&mut stream).await;
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
    let mut received = vec![];
    while received.len() < raw_audio.len() {
        let mut page = read.clone();
        page.offset = received.len() as u64;
        let chunk = raw
            .read_transcription_input(page)
            .await
            .expect("authorized audio range")
            .into_inner();
        assert_eq!(chunk.offset, received.len() as u64);
        assert_eq!(chunk.chunk_sha256, hex::encode(Sha256::digest(&chunk.data)));
        assert_eq!(chunk.version_id, available["version_id"]);
        received.extend(chunk.data);
        assert_eq!(chunk.eof, received.len() == raw_audio.len());
    }
    assert_eq!(received, raw_audio);
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
    let revokes: i64 = sqlx::query_scalar("SELECT count(*) FROM transcription_input_revocations")
        .fetch_one(db.pool())
        .await
        .expect("one revocation");
    assert_eq!(revokes, 2);
    drop(sender);
    drop(stream);
    drop(raw);
    drop(foreign);
    server.stop().await;
    store.drain().await.expect("close");
}
