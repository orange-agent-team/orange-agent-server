use super::*;
#[path = "../support/test_dispute.rs"]
mod dispute_proof;
#[path = "../support/purchase_lot.rs"]
mod purchase_lot;
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

#[derive(Clone)]
struct Buyer {
    owner: String,
    user: String,
    token: String,
    csrf: String,
    origin: String,
}
impl Buyer {
    async fn create(db: &Store, origin: &str, gift: i64) -> Self {
        let owner = Uuid::new_v4().to_string();
        let user = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
            .bind(&owner)
            .execute(db.pool())
            .await
            .expect("buyer");
        sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,$2)")
            .bind(&owner)
            .bind(gift)
            .execute(db.pool())
            .await
            .expect("fixture wallet");
        sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'input-consent',$1,$2)").bind(&user).bind(&owner).execute(db.pool()).await.expect("buyer identity");
        Self::signed_in(db, origin, owner, user).await
    }
    async fn from_device(d: &Device, origin: &str) -> Self {
        sqlx::query("INSERT INTO wallets(owner_id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(&d.owner)
            .execute(d.store.pool())
            .await
            .expect("device owner's personal wallet");
        Self::signed_in(&d.store, origin, d.owner.clone(), d.user.clone()).await
    }
    async fn signed_in(db: &Store, origin: &str, owner: String, user: String) -> Self {
        let token = Uuid::new_v4().simple().to_string();
        let csrf = Uuid::new_v4().simple().to_string();
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

async fn browser_flow(app: &Router, driver: &str, mut fixture: Value) {
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("owned API");
    let addr = listener.local_addr().expect("port");
    let (shutdown, done) = oneshot::channel();
    let router = app.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .expect("API");
    });
    fixture["api"] = json!(format!("http://{addr}"));
    let mut browser =
        tokio::process::Command::new(std::env::var("ORANGE_AUDIO_CONSENT_NODE").expect("Node"))
            .arg(driver)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("real Chrome driver");
    let mut stdin = browser.stdin.take().expect("private fixture pipe");
    stdin
        .write_all(&serde_json::to_vec(&fixture).expect("fixture"))
        .await
        .expect("deliver session");
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(85), browser.wait())
        .await
        .expect("browser deadline")
        .expect("driver exit");
    let _ = shutdown.send(());
    task.await.expect("API stopped");
    assert!(status.success(), "real browser flow");
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Scenario {
    Input,
    Publish,
    WithdrawOutput,
    InvalidOutput,
    Priced,
    CancelPriced,
    Resident,
    ResidentRevoked,
    DualRole,
}
#[tokio::test]
async fn consent_and_confirmed_lease_gate_every_real_audio_chunk() {
    run_case(Scenario::Input).await;
}
#[tokio::test]
async fn same_verified_transcript_publishes_once_and_follows_deleted_source() {
    run_case(Scenario::Publish).await;
}
#[tokio::test]
async fn verified_output_bytes_remain_private_if_consent_is_revoked_before_publication() {
    run_case(Scenario::WithdrawOutput).await;
}
#[tokio::test]
async fn inconsistent_duration_cannot_publish_or_create_a_measurement_fact() {
    run_case(Scenario::InvalidOutput).await;
}

#[tokio::test]
async fn priced_transcript_freezes_and_settles_development_credits_once() {
    run_case(Scenario::Priced).await;
}
#[tokio::test]
async fn cancelling_a_queued_transcript_releases_the_entire_reservation() {
    run_case(Scenario::CancelPriced).await;
}
#[tokio::test]
async fn foreground_provider_serves_a_priced_asr_order_and_closes_its_own_cpu_grant() {
    run_case(Scenario::Resident).await;
}

#[tokio::test]
async fn resident_lost_delivery_after_source_withdrawal_closes_the_attempt_and_refunds_full_hold() {
    run_case(Scenario::ResidentRevoked).await;
}
struct Resident {
    child: tokio::process::Child,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}
impl Resident {
    async fn wait_event(&mut self, state: &str) {
        tokio::time::timeout(Duration::from_secs(80), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .expect("worker event")
                    .expect("resident Worker exited unexpectedly");
                let event: Value = serde_json::from_str(&line).expect("event");
                eprintln!("resident event {:?} {:?}", event["state"], event["outcome"]);
                if event["state"] == state {
                    return;
                }
            }
        })
        .await
        .expect("resident Worker event deadline");
    }
    async fn stop(mut self) {
        let pid = self.child.id().expect("owned worker").to_string();
        assert!(
            tokio::process::Command::new("/bin/kill")
                .args(["-TERM", &pid])
                .status()
                .await
                .expect("stop owned Worker")
                .success()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .expect("graceful stop")
                .expect("exit")
                .success()
        );
    }
}
async fn start_resident(
    server: &Running,
    d: &Device,
    profile: &str,
    probe: &std::path::Path,
    private: &std::path::Path,
) -> Resident {
    use tokio::io::AsyncBufReadExt;
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS"));
    let text: Value = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_OBSERVATION_TEXT_PROFILE").expect("text configuration"),
        )
        .expect("profile"),
    )
    .expect("JSON");
    let state = private.join("resident");
    std::fs::create_dir(&state).expect("state");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).expect("private");
    let path = state.join("config.json");
    let cfg = json!({"protocol":1,"environment":"development","state_directory":state,"endpoint":format!("https://127.0.0.1:{}",server.port),"server_name":"localhost","server_ca":tls.join("ca.pem"),
        "certificate":tls.join(format!("{}.pem",d.certificate)),"private_key":tls.join(format!("{}.key",d.certificate)),"node_id":d.node,"allow_remote_contribution":true,
        "profile_id":"text-profile","profile":text,"transcription_observation":{"execution_profile_id":profile,"private_probe_file":probe,"allow_remote_execution":true}});
    std::fs::write(&path, serde_json::to_vec(&cfg).expect("json")).expect("config");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private file");
    let mut child = tokio::process::Command::new(
        std::env::var("ORANGE_OBSERVATION_WORKER").expect("foreground binary"),
    )
    .arg(path)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .expect("real foreground Worker");
    let lines = tokio::io::BufReader::new(child.stdout.take().expect("worker events")).lines();
    let mut resident = Resident { child, lines };
    resident.wait_event("transcription_profile_observed").await;
    resident
}
async fn balances(db: &Store, owner: &str) -> (i64, i64) {
    sqlx::query_as(
        "SELECT gift+purchased,reserved_gift+reserved_purchased FROM wallets WHERE owner_id=$1",
    )
    .bind(owner)
    .fetch_one(db.pool())
    .await
    .expect("actual wallet")
}
async fn run_case(mode: Scenario) {
    run_for(mode, None).await;
}
struct Participants {
    node: Device,
    buyer: Buyer,
    paid_order: Option<String>,
    team_budget: Option<String>,
}
impl Participants {
    fn native(node: Device, buyer: Buyer) -> Self {
        Self {
            node,
            buyer,
            paid_order: None,
            team_budget: None,
        }
    }
}
async fn run_for(mode: Scenario, participants: Option<Participants>) {
    let config = PathBuf::from(std::env::var("ORANGE_ASR_PROBE_FILE").expect("probe config"));
    let (_send, stop) = tokio::sync::watch::channel(false);
    let fact = orange_worker::host::probe_transcription(&config, stop)
        .await
        .expect("actual approved CPU profile");
    let (d, existing_buyer, known_purchase, chosen_budget) = if let Some(p) = participants {
        (p.node, Some(p.buyer), p.paid_order, p.team_budget)
    } else {
        (
            device(match mode {
                Scenario::Input => "client-s",
                Scenario::Publish => "client-v",
                Scenario::WithdrawOutput => "client-w",
                Scenario::InvalidOutput => "client-x",
                Scenario::Priced => "client-ec",
                Scenario::CancelPriced => "client-ed",
                Scenario::Resident => "client-ee",
                Scenario::ResidentRevoked => "client-ef",
                Scenario::DualRole => {
                    unreachable!("dual role case supplies both existing accounts")
                }
            })
            .await,
            None,
            None,
            None,
        )
    };
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
    let resident_funds = matches!(
        mode,
        Scenario::Resident | Scenario::ResidentRevoked | Scenario::DualRole
    );
    let buyer = match existing_buyer {
        Some(buyer) => buyer,
        None => Buyer::create(db, &cfg.user_origin, if resident_funds { 0 } else { 10000 }).await,
    };
    let purchased_order = if known_purchase.is_some() {
        known_purchase
    } else if resident_funds {
        Some(
            purchase_lot::credited(
                db.pool(),
                &buyer.owner,
                &buyer.user,
                "acct_asr_fixture",
                &Uuid::new_v4().to_string(),
                10000,
            )
            .await,
        )
    } else {
        None
    };

    let stranger = Buyer::create(db, &cfg.user_origin, 10000).await;
    let identity = UserIdentity::discover(db.pool().clone(), cfg)
        .await
        .expect("actual OIDC discovery");
    let users = orange_control_plane::auth::routes::router(identity.clone());
    let app = http::build_router(
        identity.clone(),
        store.clone(),
        Some(AudioMeter::from_file(&config).expect("meter")),
    )
    .merge(users);
    let reader = store.reader();
    let server = if mode == Scenario::Input {
        Running::start_with_input(db, Some(reader.clone())).await
    } else {
        Running::start_with_output(db, store.clone()).await
    };
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
    let mut conn = if mode == Scenario::Input {
        ControlConnection::connect_transcription_inputs(
            worker_config(&server, &d),
            &mut node,
            &mut journal,
        )
        .await
    } else if matches!(
        mode,
        Scenario::Priced
            | Scenario::CancelPriced
            | Scenario::Resident
            | Scenario::ResidentRevoked
            | Scenario::DualRole
    ) {
        ControlConnection::connect_managed_transcription(
            worker_config(&server, &d),
            &mut node,
            &mut journal,
        )
        .await
    } else {
        ControlConnection::connect_transcription_outputs(
            worker_config(&server, &d),
            &mut node,
            &mut journal,
        )
        .await
    }
    .expect("negotiated transport");
    let w = conn.identity().clone();
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,$2,$3,$4)")
      .bind(&profile).bind(fact.profile.model_version).bind(&fact.profile.model_sha256).bind(&fact.execution_profile_digest).execute(db.pool()).await.expect("independent review fixture");
    let mut observation = conn
        .report_transcription_profile(&profile, &fact)
        .await
        .expect("actual observation");
    let resident = if matches!(
        mode,
        Scenario::Resident | Scenario::ResidentRevoked | Scenario::DualRole
    ) {
        let proc = start_resident(&server, &d, &profile, &config, private.path()).await;
        observation.observation_id=sqlx::query_scalar("SELECT o.observation_id FROM worker_transcription_profile_observations o JOIN worker_sessions s USING(node_id,session_id,connection_epoch) WHERE o.node_id=$1 AND o.execution_profile_id=$2 ORDER BY o.connection_epoch DESC LIMIT 1")
            .bind(&d.node).bind(&profile).fetch_one(db.pool()).await.expect("actual foreground observation");
        Some(proc)
    } else {
        None
    };
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
    let team_order = chosen_budget.is_some();
    let budget = if let Some(root) = chosen_budget {
        db.budget(&buyer.owner, &root)
            .await
            .expect("actual allocated organizational budget")
    } else {
        db.create_budget(
            &buyer.owner,
            &Uuid::new_v4().to_string(),
            &CreateBudget {
                scope_id: Uuid::new_v4().to_string(),
                limit: "10000".to_owned().try_into().expect("budget"),
            },
        )
        .await
        .expect("budget")
    };
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
    if mode == Scenario::Input
        && let Ok(driver) = std::env::var("ORANGE_AUDIO_CONSENT_BROWSER")
    {
        let fixture = json!({"origin":buyer.origin,"token":buyer.token,"upload":upload,"node":d.node,"observation":observation.observation_id,
            "budget":budget.root_budget_id,"region":"local-test","screenshot":std::env::var("ORANGE_AUDIO_CONSENT_SCREENSHOT").expect("consent screenshot")});
        let ui = browser_flow(&app, &driver, fixture);
        tokio::pin!(ui);
        loop {
            tokio::select! {_= &mut ui=>break,_=tokio::time::sleep(Duration::from_secs(3))=>{
                conn.poll_execution("audio-input-test",&mut node,&mut journal).await.expect("maintain source session");
            }}
        }
    }
    let no_jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE owner_id=$1")
        .bind(&buyer.owner)
        .fetch_one(db.pool())
        .await
        .expect("jobs");
    assert_eq!(no_jobs, 0);
    let priced = matches!(
        mode,
        Scenario::Priced
            | Scenario::CancelPriced
            | Scenario::Resident
            | Scenario::ResidentRevoked
            | Scenario::DualRole
    );
    let price_version = Uuid::new_v4().to_string();
    let publish_gate = if mode == Scenario::ResidentRevoked {
        Some(store.pause_verified_transcript_publication().await)
    } else {
        None
    };
    let job = if priced {
        let prices = format!("{status_path}/prices");
        assert_eq!(
            buyer.call(&app, "GET", &prices, "", vec![], 200).await,
            json!([])
        );
        sqlx::query("INSERT INTO wallets(owner_id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(&d.owner)
            .execute(db.pool())
            .await
            .expect("provider development wallet fixture");
        sqlx::query("INSERT INTO transcription_prices(price_version,node_id,execution_profile_id,price_per_minute,fee_bps,review_digest,admission,enabled)
           VALUES($1,$2,$3,12000,2000,$4,'development_only',true)").bind(&price_version).bind(&d.node).bind(&profile).bind(&fact.execution_profile_digest)
           .execute(db.pool()).await.expect("explicit test-only price admission");
        assert_eq!(
            buyer.call(&app, "GET", &prices, "", vec![], 200).await,
            json!([{"price_version":price_version,"price_per_minute":"12000","fee_bps":2000}])
        );
        let id = Uuid::new_v4().to_string();
        let quote_url = format!("{base}/transcription-quotes");
        let quote_req = json!({"grant_id":grant,"price_version":price_version,"request_id":id});
        let mut forged = quote_req.clone();
        forged["duration_ms"] = json!("1");
        buyer
            .call(
                &app,
                "POST",
                &quote_url,
                &id,
                forged.to_string().into_bytes(),
                422,
            )
            .await;
        stranger
            .call(
                &app,
                "POST",
                &quote_url,
                &id,
                quote_req.to_string().into_bytes(),
                403,
            )
            .await;
        let quote = buyer
            .call(
                &app,
                "POST",
                &quote_url,
                &id,
                quote_req.to_string().into_bytes(),
                201,
            )
            .await;
        assert_eq!(
            quote,
            buyer
                .call(
                    &app,
                    "POST",
                    &quote_url,
                    &id,
                    quote_req.to_string().into_bytes(),
                    201
                )
                .await
        );
        if matches!(mode, Scenario::DualRole) {
            let quote_id = quote["quote_id"].as_str().expect("original quote");
            let preview = format!("{base}/quotes/{quote_id}/funding");
            let owner_ledger = "SELECT count(*) FROM ledger_transactions WHERE owner_id=$1";
            let before: i64 = sqlx::query_scalar(owner_ledger)
                .bind(&buyer.owner)
                .fetch_one(db.pool())
                .await
                .expect("original postings");
            let money = buyer.call(&app, "GET", &preview, "", vec![], 200).await;
            assert_eq!(money["quote_id"], quote_id);
            assert_eq!(money["max_microcredits"], quote["max_amount"]);
            assert_eq!(money["root_budget_id"], budget.root_budget_id);
            assert_eq!(money["requester_id"], buyer.user);
            assert_eq!(money["owner_id"], buyer.owner);
            assert_eq!(money["status"], "sufficient");
            if team_order {
                assert_eq!(money["wallet_available"], Value::Null);
                assert_eq!(money["account_kind"], "team");
                assert_eq!(money["budget_limit"], "3000");
            } else {
                assert_eq!(money["account_kind"], "personal");
                assert!(
                    money["wallet_available"]
                        .as_str()
                        .expect("own wallet")
                        .parse::<i64>()
                        .expect("integer")
                        >= 2200
                );
            }
            buyer
                .call(
                    &app,
                    "GET",
                    &format!("{preview}?owner_id=another-account"),
                    "",
                    vec![],
                    422,
                )
                .await;
            if team_order {
                let team_owner = Buyer::from_device(&d, &buyer.origin).await;
                assert_ne!(team_owner.user, buyer.user);
                team_owner
                    .call(&app, "GET", &preview, "", vec![], 404)
                    .await;
            }
            let unrelated = Buyer::create(db, &buyer.origin, 0).await;
            unrelated
                .call(
                    &app,
                    "GET",
                    &format!(
                        "/api/v1/owners/{}/quotes/{quote_id}/funding",
                        unrelated.owner
                    ),
                    "",
                    vec![],
                    404,
                )
                .await;
            buyer
                .call(
                    &app,
                    "GET",
                    &format!("{base}/quotes/unknown-fixture/funding"),
                    "",
                    vec![],
                    404,
                )
                .await;
            let after: i64 = sqlx::query_scalar(owner_ledger)
                .bind(&buyer.owner)
                .fetch_one(db.pool())
                .await
                .expect("read-only postings");
            assert_eq!(before, after);
        }
        assert_eq!(quote["duration_ms"], "11000");
        assert_eq!(quote["max_amount"], "2200");
        let submit_url = format!("{base}/jobs");
        let key = Uuid::new_v4().to_string();
        let body = json!({"root_budget_id":budget.root_budget_id,"invocation_id":invocation,"purpose":"transcription","quote_id":quote["quote_id"],"request_projection_digest":projection});
        let mut forged = body.clone();
        forged["purpose"] = json!("primary");
        buyer
            .call(
                &app,
                "POST",
                &submit_url,
                &key,
                forged.to_string().into_bytes(),
                403,
            )
            .await;
        let mut wrong = body.clone();
        wrong["request_projection_digest"] = json!("0".repeat(64));
        buyer
            .call(
                &app,
                "POST",
                &submit_url,
                &key,
                wrong.to_string().into_bytes(),
                409,
            )
            .await;
        sqlx::query("UPDATE transcription_prices SET enabled=false WHERE price_version=$1")
            .bind(&price_version)
            .execute(db.pool())
            .await
            .expect("withdraw price");
        buyer
            .call(
                &app,
                "POST",
                &submit_url,
                &key,
                body.to_string().into_bytes(),
                403,
            )
            .await;
        assert_eq!(balances(db, &buyer.owner).await, (10000, 0));
        sqlx::query("UPDATE transcription_prices SET enabled=true WHERE price_version=$1")
            .bind(&price_version)
            .execute(db.pool())
            .await
            .expect("restore same reviewed terms");
        let team_browser = team_order && std::env::var_os("ORANGE_AUDIO_CONSENT_BROWSER").is_some();
        let a = if team_browser {
            buyer
                .call(&app, "DELETE", &status_path, &grant, vec![], 200)
                .await;
            let driver =
                PathBuf::from(std::env::var("ORANGE_AUDIO_CONSENT_BROWSER").expect("browser"))
                    .with_file_name("real-team-compute.mjs");
            let screenshot =
                PathBuf::from(std::env::var("ORANGE_AUDIO_CONSENT_SCREENSHOT").expect("evidence"))
                    .with_file_name("team-compute-result.png");
            browser_flow(&app,driver.to_str().expect("driver"),json!({"origin":buyer.origin,"token":buyer.token,"org":buyer.owner,"budget":budget.root_budget_id,"upload":upload,"node":d.node,
                "observation":observation.observation_id,"transcript_sha":fact.transcript_sha256,"screenshot":screenshot})).await;
            let jobs:Vec<String>=sqlx::query_scalar("SELECT job_id FROM jobs WHERE owner_id=$1 AND root_budget_id=$2 AND submitting_user_id=$3").bind(&buyer.owner).bind(&budget.root_budget_id).bind(&buyer.user).fetch_all(db.pool()).await.expect("original team job");
            assert_eq!(jobs.len(), 1);
            buyer
                .call(
                    &app,
                    "GET",
                    &format!("{base}/jobs/{}", jobs[0]),
                    "",
                    vec![],
                    200,
                )
                .await
        } else if mode == Scenario::CancelPriced
            && let Ok(driver) = std::env::var("ORANGE_AUDIO_CONSENT_BROWSER")
        {
            let screenshot =
                PathBuf::from(std::env::var("ORANGE_AUDIO_CONSENT_SCREENSHOT").expect("evidence"))
                    .with_file_name("transcription-price.png");
            let flow = browser_flow(
                &app,
                &driver,
                json!({"origin":buyer.origin,"token":buyer.token,"user":buyer.user,"owner":buyer.owner,
                "invocation":invocation,"price_grant":grant,"node":d.node,"region":"local-test","budget":budget.root_budget_id,"screenshot":screenshot}),
            );
            tokio::pin!(flow);
            loop {
                tokio::select! {_=&mut flow=>break,_=tokio::time::sleep(Duration::from_secs(3))=>{
                    conn.heartbeat_node_control("audio-price-test",&mut node).await.expect("renew provider heartbeat without taking the job");
                }}
            }
            buyer
                .call(
                    &app,
                    "GET",
                    &format!("{base}/jobs/by-invocation/{invocation}"),
                    "",
                    vec![],
                    200,
                )
                .await["job"]
                .clone()
        } else {
            let (a, b) = tokio::join!(
                buyer.call(
                    &app,
                    "POST",
                    &submit_url,
                    &key,
                    body.to_string().into_bytes(),
                    200
                ),
                buyer.call(
                    &app,
                    "POST",
                    &submit_url,
                    &key,
                    body.to_string().into_bytes(),
                    200
                )
            );
            assert_eq!(a, b);
            a
        };
        assert_eq!(a["reserved"], "2200");
        if team_browser {
            assert_eq!(a["charge"], "2200");
            assert_eq!(a["execution_state"], "succeeded");
            assert_eq!(balances(db, &buyer.owner).await, (7800, 0));
        } else {
            assert_eq!(balances(db, &buyer.owner).await, (7800, 2200));
        }
        a["job_id"].as_str().expect("actual job id").to_owned()
    } else {
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
        job
    };
    if let Some(mut process) = resident {
        if let Some((arrived, resume)) = publish_gate {
            tokio::time::timeout(Duration::from_secs(70), arrived)
                .await
                .expect("provider reached publication")
                .expect("verified private bytes");
            buyer
                .call(&app, "DELETE", &status_path, &grant, vec![], 200)
                .await;
            resume
                .send(())
                .expect("allow final live authorization check");
        }
        process.wait_event("cloud_cleanup_confirmed").await;
        let proof:(String,String,String)=sqlx::query_as("SELECT r.evidence->'process'->>'pid',r.evidence->'process'->>'executable_sha256',a.state FROM worker_cpu_cleanup_receipts r JOIN worker_execution_attempts a USING(attempt_id) WHERE r.job_id=$1").bind(&job).fetch_one(db.pool()).await.expect("actual kernel-backed cleanup receipt");
        assert!(proof.0.parse::<u32>().expect("PID") > 0);
        assert_eq!(proof.1, fact.profile.runtime.python_sha256);
        assert_eq!(proof.2, "closed");
        if mode == Scenario::ResidentRevoked {
            let release:(i64,i64)=sqlx::query_as("SELECT j.charge,r.released FROM jobs j JOIN job_cleanup_releases r USING(job_id) WHERE j.job_id=$1 AND j.execution_state='failed' AND j.settlement_state='settled'")
                .bind(&job).fetch_one(db.pool()).await.expect("exact cleanup releases original hold");
            assert_eq!(release, (0, 2200));
            assert_eq!(balances(db, &buyer.owner).await, (10000, 0));
            let effects:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM job_response_commits WHERE job_id=$1)+(SELECT count(*) FROM job_settlements WHERE job_id=$1)+(SELECT count(*) FROM transcription_response_facts WHERE job_id=$1)")
                .bind(&job).fetch_one(db.pool()).await.expect("no output or income");
            assert_eq!(effects, 0);
            let earned: i64 =
                sqlx::query_scalar("SELECT pending_earned FROM wallets WHERE owner_id=$1")
                    .bind(&d.owner)
                    .fetch_one(db.pool())
                    .await
                    .expect("no provider income");
            assert_eq!(earned, 0);
        } else {
            let settled: (i64, i64, i64) = sqlx::query_as(
                "SELECT charge,platform_fee,provider_pending FROM job_settlements WHERE job_id=$1",
            )
            .bind(&job)
            .fetch_one(db.pool())
            .await
            .expect("real provider settlement");
            assert_eq!(settled, (2200, 440, 1760));
            let result = buyer
                .call(
                    &app,
                    "GET",
                    &format!("{base}/jobs/{job}/result"),
                    "",
                    vec![],
                    200,
                )
                .await;
            let content = buyer
                .call(
                    &app,
                    "GET",
                    &format!(
                        "{base}/uploads/{}/content?version_id={}",
                        result["upload_id"].as_str().expect("upload"),
                        result["version_id"].as_str().expect("version")
                    ),
                    "",
                    vec![],
                    200,
                )
                .await;
            assert_eq!(
                hex::encode(Sha256::digest(
                    content["text"].as_str().expect("transcript").as_bytes()
                )),
                fact.transcript_sha256
            );
            assert_eq!(balances(db, &buyer.owner).await, (7800, 0));
        }
        let pins: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM artifact_pins WHERE reference_id=$1 OR reference_id=$2",
        )
        .bind(format!("job-input:{job}"))
        .bind(format!("job-output:{job}"))
        .fetch_one(db.pool())
        .await
        .expect("post-receipt pin release");
        assert_eq!(pins, 0);
        let lot: (i64, i64, i64) = sqlx::query_as(
            "SELECT available,job_held,spent FROM test_purchase_lots WHERE order_id=$1",
        )
        .bind(
            purchased_order
                .as_deref()
                .expect("original purchase fixture"),
        )
        .fetch_one(db.pool())
        .await
        .expect("actual purchased bucket attribution");
        assert_eq!(
            lot,
            if mode != Scenario::ResidentRevoked {
                (7800, 0, 2200)
            } else {
                (10000, 0, 0)
            }
        );
        if mode == Scenario::Resident {
            dispute_proof::after_actual_spend(
                db,
                &identity,
                &buyer.owner,
                purchased_order.as_deref().expect("purchase"),
                &job,
            )
            .await;
        }
        if team_order {
            buyer
                .call(&app, "DELETE", &endpoint, upload, vec![], 200)
                .await;
            buyer
                .call(
                    &app,
                    "GET",
                    &format!("{base}/jobs/{job}/result"),
                    "",
                    vec![],
                    403,
                )
                .await;
        }
        process.stop().await;
        drop(conn);
        drop(raw);
        server.stop().await;
        store.drain().await.expect("objects closed");
        return;
    }
    if mode == Scenario::CancelPriced {
        let url = format!("{base}/jobs/{job}/cancel");
        let key = Uuid::new_v4().to_string();
        buyer
            .call(&app, "POST", &url, &key, b"{}".to_vec(), 200)
            .await;
        buyer
            .call(&app, "POST", &url, &key, b"{}".to_vec(), 200)
            .await;
        assert_eq!(balances(db, &buyer.owner).await, (10000, 0));
        let budget_amounts: (i64, i64) =
            sqlx::query_as("SELECT spent,reserved FROM budget_roots WHERE root_budget_id=$1")
                .bind(&budget.root_budget_id)
                .fetch_one(db.pool())
                .await
                .expect("released budget");
        assert_eq!(budget_amounts, (0, 0));
        let charges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM job_settlements WHERE job_id=$1")
                .bind(&job)
                .fetch_one(db.pool())
                .await
                .expect("no income");
        assert_eq!(charges, 0);
        drop(conn);
        drop(raw);
        server.stop().await;
        store.drain().await.expect("store closed");
        return;
    }
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
    let transcript = candidate.transcript.as_ref().expect("complete candidate");
    assert_eq!(transcript.language, "en");
    assert_eq!(
        hex::encode(Sha256::digest(transcript.text.as_bytes())),
        fact.transcript_sha256
    );
    if mode != Scenario::Input {
        use sqlx::Row;
        let mut result = received
            .output(candidate, &fact.profile.compute_type)
            .expect("checked transcript");
        assert!(
            conn.commit_transcription_output(&mut journal, &record.command_id, &result)
                .await
                .is_err(),
            "local output cannot bypass its durable CPU cleanup evidence"
        );
        if mode == Scenario::InvalidOutput {
            result.measurement.duration_ms = "1".into();
        }
        let bytes = serde_json::to_vec(&result).expect("result bytes");
        let recorded=sqlx::query("SELECT execution_journal_id,broker_grant_id,broker_epoch,slot_generation FROM worker_execution_attempts WHERE command_id=$1")
            .bind(&record.command_id).fetch_one(db.pool()).await.expect("actual accepted ACK identity");
        let ack = worker::Acknowledge {
            command_id: record.command_id.clone(),
            payload_digest: assignment.payload_digest.clone(),
            lease: assignment.lease.clone(),
            result: worker::AckResult::Accepted as i32,
            execution_journal_id: recorded.get("execution_journal_id"),
            broker_grant_id: recorded.get("broker_grant_id"),
            broker_epoch: recorded.get::<i64, _>("broker_epoch") as u64,
            slot_generation: recorded.get::<i64, _>("slot_generation") as u64,
        };
        let h = worker::OutputHeader {
            transcription_output_version: 1,
            session_id: w.session_id.clone(),
            upload_id: Uuid::new_v4().to_string(),
            accepted: Some(ack),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            response_digest: orange_protocol::request_digest(
                "worker.transcription.result.v1",
                &result,
            )
            .expect("digest"),
        };
        let frames = || {
            vec![
                worker::OutputFrame {
                    body: Some(worker::output_frame::Body::Header(h.clone())),
                },
                worker::OutputFrame {
                    body: Some(worker::output_frame::Body::Data(worker::OutputData {
                        offset: 0,
                        data: bytes.clone(),
                    })),
                },
            ]
        };
        let mut wrong = frames();
        if let Some(worker::output_frame::Body::Header(header)) = &mut wrong[0].body {
            header.transcription_output_version = 0;
        }
        assert!(
            raw.commit_output(tokio_stream::iter(wrong)).await.is_err(),
            "text header cannot commit audio"
        );
        let job_result = format!("{base}/jobs/{job}/result");
        if matches!(mode, Scenario::Publish | Scenario::Priced) {
            let receipt = raw
                .commit_output(tokio_stream::iter(frames()))
                .await
                .expect("actual transcript published")
                .into_inner();
            assert_eq!(
                receipt.artifact.as_ref().expect("artifact").media_type,
                orange_protocol::transcript::MEDIA
            );
            let query = worker::OutputQuery {
                session_id: w.session_id.clone(),
                original: Some(h.clone()),
            };
            assert_eq!(
                raw.query_output(query)
                    .await
                    .expect("receipt lookup")
                    .into_inner(),
                receipt
            );
            assert_eq!(
                raw.commit_output(tokio_stream::iter(frames()))
                    .await
                    .expect("same immutable upload")
                    .into_inner(),
                receipt
            );
            if priced {
                let actual:(i64,i64,i64)=sqlx::query_as("SELECT charge,platform_fee,provider_pending FROM job_settlements WHERE job_id=$1 AND provider_owner_id=$2")
                    .bind(&job).bind(&d.owner).fetch_one(db.pool()).await.expect("actual immutable settlement");
                assert_eq!(actual, (2200, 440, 1760));
                assert_eq!(balances(db, &buyer.owner).await, (7800, 0));
                let earned: i64 =
                    sqlx::query_scalar("SELECT pending_earned FROM wallets WHERE owner_id=$1")
                        .bind(&d.owner)
                        .fetch_one(db.pool())
                        .await
                        .expect("provider wallet");
                assert_eq!(earned, 1760);
                let budget_amounts: (i64, i64) = sqlx::query_as(
                    "SELECT spent,reserved FROM budget_roots WHERE root_budget_id=$1",
                )
                .bind(&budget.root_budget_id)
                .fetch_one(db.pool())
                .await
                .expect("settled budget");
                assert_eq!(budget_amounts, (2200, 0));
                let ledger:(i64,i64,i64)=sqlx::query_as("SELECT count(DISTINCT transaction_id),sum(amount)::bigint,sum(amount) FILTER(WHERE account='platform:fees')::bigint FROM ledger_transactions JOIN ledger_entries USING(transaction_id) WHERE business_key=$1")
                    .bind(format!("settle:{job}")).fetch_one(db.pool()).await.expect("ledger balancing");
                assert_eq!(ledger, (1, 0, 440));
            }
            let v = buyer.call(&app, "GET", &job_result, "", vec![], 200).await;
            let url = format!(
                "{base}/uploads/{}/content?version_id={}",
                h.upload_id,
                v["version_id"].as_str().expect("version")
            );
            let delivered = buyer.call(&app, "GET", &url, "", vec![], 200).await;
            assert_eq!(
                delivered,
                serde_json::to_value(&result).expect("published document")
            );
            let fact_row=sqlx::query("SELECT duration_ms,source_sha256,response_digest FROM transcription_response_facts WHERE job_id=$1").bind(&job).fetch_one(db.pool()).await.expect("one immutable fact");
            assert_eq!(fact_row.get::<i64, _>("duration_ms"), 11000);
            assert_eq!(
                fact_row.get::<String, _>("response_digest"),
                h.response_digest
            );
            assert_eq!(
                fact_row.get::<String, _>("source_sha256"),
                fact.sample_sha256
            );
            if let Ok(driver) = std::env::var("ORANGE_AUDIO_CONSENT_BROWSER") {
                let path = PathBuf::from(
                    std::env::var("ORANGE_AUDIO_CONSENT_SCREENSHOT").expect("evidence"),
                )
                .with_file_name("transcript-result.png");
                browser_flow(&app,&driver,json!({"origin":buyer.origin,"token":buyer.token,"csrf":buyer.csrf,"owner":buyer.owner,"upload":upload,
                    "job":job,"transcript_sha":fact.transcript_sha256,"segments":result.segments.len(),"screenshot":path})).await;
            }
            buyer
                .call(&app, "DELETE", &endpoint, upload, vec![], 200)
                .await;
            buyer.call(&app, "GET", &job_result, "", vec![], 403).await;
            assert!(
                raw.query_output(worker::OutputQuery {
                    session_id: w.session_id.clone(),
                    original: Some(h.clone())
                })
                .await
                .is_err(),
                "historical Worker receipt still requires a readable source"
            );
            store
                .collect(50)
                .await
                .expect("derived artifact collection");
            let state: String =
                sqlx::query_scalar("SELECT state FROM artifacts WHERE artifact_id=$1")
                    .bind(v["artifact_id"].as_str().expect("id"))
                    .fetch_one(db.pool())
                    .await
                    .expect("retired");
            assert_eq!(state, "deleted");
        } else {
            if mode == Scenario::WithdrawOutput {
                let (arrived, resume) = store.pause_verified_transcript_publication().await;
                let mut client = raw.clone();
                let data = frames();
                let output =
                    tokio::spawn(
                        async move { client.commit_output(tokio_stream::iter(data)).await },
                    );
                tokio::time::timeout(Duration::from_secs(4), arrived)
                    .await
                    .expect("verified output written privately")
                    .expect("gate");
                buyer
                    .call(
                        &app,
                        "DELETE",
                        &format!("{path}/{grant}"),
                        &grant,
                        vec![],
                        200,
                    )
                    .await;
                resume.send(()).expect("resume final publication");
                assert!(output.await.expect("joined").is_err());
            } else {
                assert!(
                    raw.commit_output(tokio_stream::iter(frames()))
                        .await
                        .is_err(),
                    "invalid decoded usage"
                );
            }
            let facts:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM job_response_commits WHERE job_id=$1)+(SELECT count(*) FROM transcription_response_facts WHERE job_id=$1)").bind(&job).fetch_one(db.pool()).await.expect("no effects");
            assert_eq!(facts, 0);
            buyer.call(&app, "GET", &job_result, "", vec![], 409).await;
            store.collect(50).await.expect("orphan transfer collected");
        }
        if matches!(mode, Scenario::WithdrawOutput | Scenario::InvalidOutput) {
            let removed:Vec<(String,String,String,bool,bool,i64)>=sqlx::query_as("SELECT t.stage_key,t.published_key,t.state,t.stage_deleted,t.published_deleted,t.reserved_bytes FROM artifact_transfers t JOIN worker_output_uploads w USING(artifact_id) WHERE w.job_id=$1")
                .bind(&job).fetch_all(db.pool()).await.expect("private object cleanup");
            assert_eq!(removed.len(), 1);
            for (stage, published, state, stage_deleted, published_deleted, reserved) in removed {
                assert_eq!(state, "purged");
                assert!(stage_deleted && published_deleted);
                assert_eq!(reserved, 0);
                for key in [stage, published] {
                    assert_eq!(
                        std::fs::metadata(root.path().join(key))
                            .expect_err("private bytes actually removed")
                            .kind(),
                        std::io::ErrorKind::NotFound
                    );
                }
            }
        }
        drop(conn);
        drop(raw);
        drop(foreign);
        server.stop().await;
        store.drain().await.expect("writer closed");
        return;
    }
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
    let historical =
        historical_gateway::HistoricalGateway::start(51, &url, Some(root.path())).await;
    let tmp = tempfile::tempdir().expect("journals");
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private state");
    let mut node =
        NodeJournal::open(&tmp.path().join("node.sqlite"), d.node.clone()).expect("node");
    let mut journal =
        ExecutionJournal::open(&tmp.path().join("exec.sqlite"), d.node.clone()).expect("execution");
    let mut old = ControlConnection::connect_execution_with_inputs(
        worker_config_at(historical.port, &d),
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
    let before: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
        .fetch_one(db.pool())
        .await
        .expect("real historical schema");
    assert_eq!(
        before, 51,
        "Historical gateway unexpectedly changed the schema"
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
    let server = Running::start_with_input(&db, Some(store.reader())).await;
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
    historical.stop().await;
    server.stop().await;
    store.drain().await.expect("close");
}

#[tokio::test]
async fn two_accounts_alternate_provider_and_buyer_without_spending_pending_income_or_team_funds() {
    let alice = device("client-ei").await;
    let bob = device("client-ej").await;
    let db = alice.store.clone();
    let cfg: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity config"))
            .expect("config"),
    )
    .expect("OIDC");
    let consumer_a = Buyer::from_device(&alice, &cfg.user_origin).await;
    let consumer_b = Buyer::from_device(&bob, &cfg.user_origin).await;
    let a = alice.owner.clone();
    let b = bob.owner.clone();
    let team = orange_control_plane::team::TeamStore::new(db.pool().clone())
        .create_organization(
            &alice.user,
            "dual-team",
            &orange_protocol::team::CreateOrganization {
                name: "Dual-role development team".into(),
                member_limit: 3,
            },
        )
        .await
        .expect("real team creation")
        .organization_id;
    sqlx::query("UPDATE wallets SET gift=137 WHERE owner_id=$1")
        .bind(&team)
        .execute(db.pool())
        .await
        .expect("separate team fixture balance");
    run_for(
        Scenario::DualRole,
        Some(Participants::native(alice, consumer_b)),
    )
    .await;
    let funding: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT purchased,gift,earned,pending_earned FROM wallets WHERE owner_id=$1",
    )
    .bind(&a)
    .fetch_one(db.pool())
    .await
    .expect("provider earnings");
    assert_eq!(funding, (0, 0, 0, 1760));
    let root = db
        .create_budget(
            &a,
            "no-pending-spend",
            &CreateBudget {
                scope_id: "dual-role-spend".into(),
                limit: "1000".to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("budget");
    let invoke = Uuid::new_v4().to_string();
    let quote = Uuid::new_v4().to_string();
    let projection = "d".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'dual-role-test',100,clock_timestamp()+interval '5 minutes')")
        .bind(&quote).bind(&a).bind(&root.root_budget_id).bind(&invoke).bind(&projection).execute(db.pool()).await.expect("trusted eligibility fixture");
    let result = db
        .submit(
            &a,
            "attempt-pending-income",
            &SubmitJob {
                root_budget_id: root.root_budget_id,
                invocation_id: invoke,
                quote_id: quote,
                request_projection_digest: projection,
                purpose: InvocationPurpose::Primary,
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(orange_control_plane::store::StoreError::Balance)
    ));
    run_for(
        Scenario::DualRole,
        Some(Participants::native(bob, consumer_a)),
    )
    .await;
    for (consumer, provider) in [(&a, &b), (&b, &a)] {
        let actual:(i64,i64,i64)=sqlx::query_as("SELECT s.charge,s.platform_fee,s.provider_pending FROM job_settlements s JOIN jobs j USING(job_id) WHERE j.owner_id=$1 AND s.provider_owner_id=$2")
           .bind(consumer).bind(provider).fetch_one(db.pool()).await.expect("cross-account delivery");
        assert_eq!(actual, (2200, 440, 1760));
        let balances:(i64,i64,i64,i64)=sqlx::query_as("SELECT purchased,reserved_purchased,earned,pending_earned FROM wallets WHERE owner_id=$1")
            .bind(consumer).fetch_one(db.pool()).await.expect("independent spending and income");
        assert_eq!(balances, (7800, 0, 0, 1760));
    }
    let untouched: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT gift,purchased,reserved_gift,pending_earned FROM wallets WHERE owner_id=$1",
    )
    .bind(&team)
    .fetch_one(db.pool())
    .await
    .expect("team wallet");
    assert_eq!(untouched, (137, 0, 0, 0));
}

#[path = "../support/refund_admin.rs"]
mod journey_admin;
#[path = "../support/stripe_merchant.rs"]
mod journey_merchant;

async fn purchase_with_original_user(b: &Buyer, app: &Router) -> String {
    let id = Uuid::new_v4().to_string();
    let base = format!("/api/v1/owners/{}/checkout-orders", b.owner);
    b.call(
        app,
        "POST",
        &base,
        &id,
        json!({"order_id":id,"offer_id":"journey-pack"})
            .to_string()
            .into_bytes(),
        201,
    )
    .await;
    let session = b
        .call(
            app,
            "POST",
            &format!("{base}/{id}/session"),
            &id,
            b"{}".to_vec(),
            200,
        )
        .await;
    assert_eq!(session["state"], "open");
    let credited = b
        .call(
            app,
            "POST",
            &format!("{base}/{id}/reconcile"),
            &id,
            b"{}".to_vec(),
            200,
        )
        .await;
    assert_eq!(credited["state"], "credited");
    assert_eq!(credited["offer"]["microcredits"], "10000");
    id
}
#[tokio::test]
async fn one_no_gpu_account_traverses_consumption_supply_team_privacy_purchase_and_refund() {
    use orange_control_plane::payments::{Payments, Sandbox};
    let node_a = device("client-ek").await;
    let node_b = device("client-el").await;
    let db = node_a.store.clone();
    let cfg: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity config"))
            .expect("file"),
    )
    .expect("OIDC");
    let a = Buyer::from_device(&node_a, &cfg.user_origin).await;
    let b = Buyer::from_device(&node_b, &cfg.user_origin).await;
    let identity = UserIdentity::discover(db.pool().clone(), cfg)
        .await
        .expect("real discovery");
    let (merchant, addr, stop, server) = journey_merchant::start().await;
    let payments = Payments::new(
        identity.clone(),
        Sandbox::loopback_fixture(journey_merchant::config(), addr).expect("local merchant"),
    );
    let app = orange_control_plane::auth::routes::router(identity).merge(payments.router());
    sqlx::query("INSERT INTO test_credit_offers VALUES($1,'journey-pack','price_devUnits','usd',500,10000,true)").bind(journey_merchant::MERCHANT).execute(db.pool()).await.expect("catalogue fixture");
    let team = a
        .call(
            &app,
            "POST",
            "/api/v1/organizations",
            "six-journey-org",
            json!({"name":"Six journeys","member_limit":3})
                .to_string()
                .into_bytes(),
            200,
        )
        .await;
    let org = team["organization_id"].as_str().expect("team").to_owned();
    let invite = Uuid::new_v4().to_string();
    let manage = format!("/api/v1/organizations/{org}");
    a.call(&app,"POST",&format!("{manage}/invitations"),&invite,json!({"invitation_id":invite,"recipient_id":b.user,"role":"member","ttl_seconds":600,"expected_policy_version":team["policy_version"]}).to_string().into_bytes(),200).await;
    b.call(
        &app,
        "POST",
        &format!("/api/v1/team-invitations/{invite}/accept"),
        &invite,
        br#"{"accept":true}"#.to_vec(),
        200,
    )
    .await;
    let policy = a
        .call(
            &app,
            "POST",
            &format!("{manage}/budget-policy"),
            "journey-policy",
            br#"{"expected_version":"0","owner_ceiling":"10000","hard_limit":"3000"}"#.to_vec(),
            200,
        )
        .await;
    let root = Uuid::new_v4().to_string();
    a.call(&app,"POST",&format!("{manage}/budgets"),&root,json!({"budget_id":root,"member_id":b.user,"limit":"3000","expected_policy_version":policy["version"]}).to_string().into_bytes(),200).await;
    // Only the route scope changes. The original user cookies always authenticate Alice or Bob.
    let mut finance_a = a.clone();
    finance_a.owner = org.clone();
    let mut team_b = b.clone();
    team_b.owner = org.clone();
    let team_purchase = purchase_with_original_user(&finance_a, &app).await;
    run_for(
        Scenario::DualRole,
        Some(Participants {
            node: node_a,
            buyer: team_b,
            paid_order: Some(team_purchase),
            team_budget: Some(root.clone()),
        }),
    )
    .await;
    let used = b
        .call(
            &app,
            "GET",
            &format!("{manage}/budgets/{root}"),
            "",
            vec![],
            200,
        )
        .await;
    assert_eq!(used["spent"], "2200");
    assert_eq!(used["reserved"], "0");
    assert_eq!(balances(&db, &b.owner).await, (0, 0));
    let personal = purchase_with_original_user(&a, &app).await;
    run_for(
        Scenario::DualRole,
        Some(Participants {
            node: node_b,
            buyer: a.clone(),
            paid_order: Some(personal.clone()),
            team_budget: None,
        }),
    )
    .await;
    let refund = Uuid::new_v4().to_string();
    let base = format!("/api/v1/owners/{}", a.owner);
    let r = a
        .call(
            &app,
            "POST",
            &format!("{base}/checkout-orders/{personal}/refunds"),
            &refund,
            json!({"refund_id":refund,"currency":"usd","amount_minor":"100"})
                .to_string()
                .into_bytes(),
            201,
        )
        .await;
    assert_eq!(r["held_microcredits"], "2000");
    journey_admin::Admin::new(&db, &a.owner)
        .await
        .approve(&a.owner, &r)
        .await;
    assert!(
        payments
            .process_refund()
            .await
            .expect("approved remote request")
    );
    let remote = format!("re_{}", refund.replace('-', ""));
    merchant
        .refunds
        .records
        .lock()
        .expect("merchant")
        .get_mut(&remote)
        .expect("issued refund")["status"] = json!("succeeded");
    assert!(
        payments
            .process_refund()
            .await
            .expect("independent read of exact refund")
    );
    let final_state = a
        .call(
            &app,
            "GET",
            &format!("{base}/refunds/{refund}"),
            "",
            vec![],
            200,
        )
        .await;
    assert_eq!(final_state["state"], "succeeded");
    assert_eq!(balances(&db, &a.owner).await, (5800, 0));
    assert_eq!(balances(&db, &org).await, (7800, 0));
    assert_eq!(balances(&db, &b.owner).await, (0, 0));
    for owner in [&a.owner, &b.owner] {
        let buckets: (i64, i64) =
            sqlx::query_as("SELECT earned,pending_earned FROM wallets WHERE owner_id=$1")
                .bind(owner)
                .fetch_one(db.pool())
                .await
                .expect("provider income");
        assert_eq!(buckets, (0, 1760));
    }
    let recorded:i64=sqlx::query_scalar("SELECT count(*) FROM job_settlements s JOIN jobs j USING(job_id) WHERE (j.owner_id=$1 AND s.provider_owner_id=$2) OR (j.owner_id=$2 AND s.provider_owner_id=$3)").bind(&org).bind(&a.owner).bind(&b.owner).fetch_one(db.pool()).await.expect("reciprocal accounting");
    assert_eq!(recorded, 2);
    statement_accounting::verify_days(&db, &payments, &org, &a.owner, &b.owner).await;
    if std::env::var_os("ORANGE_PROVIDER_EARNING_TEST").is_some() {
        earning_review::after_actual_jobs(&db, &a, &b, &app, &org).await;
    }
    let _ = stop.send(());
    server.await.expect("merchant stopped");
}

#[path = "earning_review.rs"]
mod earning_review;

#[path = "statement_accounting.rs"]
mod statement_accounting;
