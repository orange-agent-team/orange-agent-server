use super::*;
use orange_worker::{
    control::ControlConnection, execution_journal::ExecutionJournal, node_journal::NodeJournal,
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn actual_profile_registration_is_idempotent_current_and_has_no_execution_authority() {
    let (_stop, cancel) = tokio::sync::watch::channel(false);
    let config =
        PathBuf::from(std::env::var("ORANGE_ASR_PROBE_FILE").expect("private probe config"));
    let fact = orange_worker::host::probe_transcription(&config, cancel)
        .await
        .expect("actual public speech on approved CPU engine");
    let d = device("client-s").await;
    let server = Running::start(&d.store).await;
    let temp = tempfile::tempdir().expect("journal root");
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private journals");
    let mut node =
        NodeJournal::open(&temp.path().join("node.sqlite"), d.node.clone()).expect("node");
    let mut journal = ExecutionJournal::open(&temp.path().join("execution.sqlite"), d.node.clone())
        .expect("journal");
    let mut connection =
        ControlConnection::connect_execution(worker_config(&server, &d), &mut node, &mut journal)
            .await
            .expect("actual mTLS execution session");
    let profile = Uuid::new_v4().to_string();
    assert!(
        connection
            .report_transcription_profile(&profile, &fact)
            .await
            .is_err(),
        "device cannot approve its own model"
    );
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest)
        VALUES($1,$2,$3,$4)").bind(&profile).bind(fact.profile.model_version).bind(&fact.profile.model_sha256)
        .bind(&fact.execution_profile_digest).execute(d.store.pool()).await.expect("separate development review fixture");
    let receipt = connection
        .report_transcription_profile(&profile, &fact)
        .await
        .expect("authentic observation");
    let replay = connection
        .report_transcription_profile(&profile, &fact)
        .await
        .expect("receipt replay");
    assert_eq!(receipt, replay);
    let mut raw = server
        .client(Some(&d.certificate))
        .await
        .expect("actual certificate");
    let req = worker::TranscriptionObservation {
        observation_version: 1,
        session_id: receipt.session_id.clone(),
        request_id: receipt.request_id.clone(),
        execution_profile_id: profile.clone(),
        execution_profile_digest: fact.execution_profile_digest.clone(),
        model_version: fact.profile.model_version.into(),
        model_sha256: fact.profile.model_sha256.clone(),
        sample_sha256: fact.sample_sha256.clone(),
        duration_ms: fact.sample_duration_ms,
        normalized_samples: fact.normalized_samples,
        transcript_sha256: fact.transcript_sha256.clone(),
        compute_type: fact.profile.compute_type.clone(),
        profile_descriptor_json: serde_json::to_vec(&fact.profile).expect("actual descriptor"),
    };
    let mut parallel = raw.clone();
    let (a, b) = tokio::join!(
        raw.report_transcription_observation(req.clone()),
        parallel.report_transcription_observation(req.clone())
    );
    assert_eq!(
        a.expect("duplicate A").into_inner(),
        b.expect("duplicate B").into_inner()
    );
    let mut invalid = req.clone();
    invalid.duration_ms += 1;
    assert_eq!(
        raw.report_transcription_observation(invalid)
            .await
            .expect_err("false decoded duration")
            .code(),
        tonic::Code::InvalidArgument
    );
    let mut changed_mode = req.clone();
    changed_mode.compute_type = if req.compute_type == "int8" {
        "int8_float32".into()
    } else {
        "int8".into()
    };
    assert_eq!(
        raw.report_transcription_observation(changed_mode)
            .await
            .expect_err("wrong observed computation for this digest")
            .code(),
        tonic::Code::InvalidArgument
    );
    let mut wrong = req.clone();
    let mut different: serde_json::Value =
        serde_json::from_slice(&req.profile_descriptor_json).expect("profile");
    different["runner_sha256"] = serde_json::json!("0".repeat(64));
    wrong.execution_profile_digest =
        orange_protocol::request_digest("worker.transcription-profile.v1", &different)
            .expect("changed digest");
    wrong.profile_descriptor_json = serde_json::to_vec(&different).expect("changed descriptor");
    assert_eq!(
        raw.report_transcription_observation(wrong)
            .await
            .expect_err("unapproved runtime")
            .code(),
        tonic::Code::FailedPrecondition
    );
    let mut other_key = req.clone();
    other_key.request_id = Uuid::new_v4().to_string();
    assert_eq!(
        raw.report_transcription_observation(other_key)
            .await
            .expect_err("second receipt for this profile/epoch")
            .code(),
        tonic::Code::AlreadyExists
    );
    let alias = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,$2,$3,$4)")
        .bind(&alias).bind(fact.profile.model_version).bind(&fact.profile.model_sha256).bind(&fact.execution_profile_digest)
        .execute(d.store.pool()).await.expect("second approved profile fixture");
    let mut changed_id = req.clone();
    changed_id.execution_profile_id = alias;
    assert_eq!(
        raw.report_transcription_observation(changed_id)
            .await
            .expect_err("one request cannot refer to a different approved profile")
            .code(),
        tonic::Code::AlreadyExists
    );
    let mut foreign = server
        .client(Some("client-t"))
        .await
        .expect("different trusted-CA device key");
    assert!(
        foreign
            .report_transcription_observation(req.clone())
            .await
            .is_err()
    );
    let mut duplicates = req.clone();
    let raw_profile = String::from_utf8(duplicates.profile_descriptor_json.clone()).expect("UTF-8");
    duplicates.profile_descriptor_json = raw_profile
        .replacen('{', r#"{"device":"cpu","#, 1)
        .into_bytes();
    assert_eq!(
        raw.report_transcription_observation(duplicates)
            .await
            .expect_err("duplicate descriptor keys")
            .code(),
        tonic::Code::InvalidArgument
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worker_transcription_profile_observations WHERE node_id=$1",
    )
    .bind(&d.node)
    .fetch_one(d.store.pool())
    .await
    .expect("count");
    assert_eq!(count, 1);
    let mut fresh =
        ControlConnection::connect_execution(worker_config(&server, &d), &mut node, &mut journal)
            .await
            .expect("superseding connection");
    assert!(fresh.identity().connection_epoch > receipt.connection_epoch);
    assert!(
        raw.report_transcription_observation(req.clone())
            .await
            .is_err(),
        "previous epoch cannot repeat a receipt"
    );
    let second = fresh
        .report_transcription_profile(&profile, &fact)
        .await
        .expect("fresh observation");
    assert_ne!(second.observation_id, receipt.observation_id);
    sqlx::query(
        "UPDATE reviewed_execution_profiles SET enabled=false WHERE execution_profile_id=$1",
    )
    .bind(&profile)
    .execute(d.store.pool())
    .await
    .expect("withdraw active profile");
    assert!(
        fresh
            .report_transcription_profile(&profile, &fact)
            .await
            .is_err(),
        "disabled review suppresses even repeat access"
    );
    assert!(sqlx::query("UPDATE worker_transcription_profile_observations SET compute_type=compute_type WHERE observation_id=$1")
        .bind(&receipt.observation_id).execute(d.store.pool()).await.is_err());
    assert!(
        sqlx::query(
            "DELETE FROM worker_transcription_profile_observations WHERE observation_id=$1"
        )
        .bind(&receipt.observation_id)
        .execute(d.store.pool())
        .await
        .is_err(),
        "immutable audit"
    );
    sqlx::query(
        "UPDATE reviewed_execution_profiles SET enabled=true WHERE execution_profile_id=$1",
    )
    .bind(&profile)
    .execute(d.store.pool())
    .await
    .expect("independent credential check");
    sqlx::query("UPDATE worker_credentials SET active=false WHERE certificate_sha256=$1")
        .bind(&d.fingerprint)
        .execute(d.store.pool())
        .await
        .expect("revoke certificate");
    assert!(
        fresh
            .report_transcription_profile(&profile, &fact)
            .await
            .is_err(),
        "revoked credential cannot recover an observation"
    );
    let history: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worker_transcription_profile_observations WHERE node_id=$1",
    )
    .bind(&d.node)
    .fetch_one(d.store.pool())
    .await
    .expect("retained history");
    assert_eq!(history, 2);
    let effects:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM jobs)+(SELECT count(*) FROM quotes)+(SELECT count(*) FROM ledger_transactions)")
        .fetch_one(d.store.pool()).await.expect("no financial effects");
    assert_eq!(effects, 0);
    drop(fresh);
    drop(connection);
    drop(raw);
    drop(parallel);
    drop(foreign);
    server.stop().await;
}

#[tokio::test]
async fn foreground_worker_reports_only_after_explicit_configuration() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    let (_send, stop) = tokio::sync::watch::channel(false);
    let probe =
        PathBuf::from(std::env::var("ORANGE_ASR_PROBE_FILE").expect("fixed probe settings"));
    let reviewed = orange_worker::host::probe_transcription(&probe, stop)
        .await
        .expect("separate local review sample");
    let d = device("client-t").await;
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,$2,$3,$4)")
        .bind(&profile).bind(reviewed.profile.model_version).bind(reviewed.profile.model_sha256)
        .bind(&reviewed.execution_profile_digest).execute(d.store.pool()).await.expect("approved fixture profile");
    let private = tempfile::tempdir().expect("workspace");
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private root");
    let obj = private.path().join("objects");
    std::fs::create_dir(&obj).expect("root");
    std::fs::set_permissions(&obj, std::fs::Permissions::from_mode(0o700))
        .expect("private objects");
    let writer = orange_control_plane::artifacts::ArtifactStore::open_local(
        d.store.pool().clone(),
        &obj,
        1048576,
        100,
    )
    .await
    .expect("object root");
    let server = Running::start_with_output(&d.store, writer.clone()).await;
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("private TLS"));
    let text_profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_OBSERVATION_TEXT_PROFILE").expect("actual text profile"),
        )
        .expect("file"),
    )
    .expect("profile json");
    for mode in ["regular", "offered", "withdrawn"] {
        let offered = mode != "regular";
        if mode == "withdrawn" {
            sqlx::query("UPDATE reviewed_execution_profiles SET enabled=false WHERE execution_profile_id=$1").bind(&profile).execute(d.store.pool()).await.expect("review withdrawn");
        }
        let state = private.path().join(mode);
        std::fs::create_dir(&state).expect("state");
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))
            .expect("private state");
        let mut config = serde_json::json!({"protocol":1,"environment":"development","state_directory":state,
            "endpoint":format!("https://127.0.0.1:{}",server.port),"server_name":"localhost","server_ca":tls.join("ca.pem"),
            "certificate":tls.join("client-t.pem"),"private_key":tls.join("client-t.key"),"node_id":d.node,
            "allow_remote_contribution":true,"profile_id":"text-profile","profile":text_profile});
        if offered {
            config["transcription_observation"] =
                serde_json::json!({"execution_profile_id":profile,"private_probe_file":probe});
        }
        let path = state.join("config.json");
        std::fs::write(&path, serde_json::to_vec(&config).expect("json")).expect("config");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private config");
        let mut child = Command::new(
            std::env::var("ORANGE_OBSERVATION_WORKER").expect("built foreground Worker"),
        )
        .arg(&path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("real Worker");
        let mut lines = BufReader::new(child.stdout.take().expect("output")).lines();
        let event = if mode == "withdrawn" {
            "transcription_profile_not_admitted"
        } else if offered {
            "transcription_profile_observed"
        } else {
            "ready"
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let text = lines.next_line().await.expect("event").expect("live owner");
                let value: serde_json::Value = serde_json::from_str(&text).expect("event json");
                if value["state"] == event {
                    break;
                }
            }
        })
        .await
        .expect("real Worker readiness and report");
        let pid = child.id().expect("owned process").to_string();
        assert!(
            Command::new("/bin/kill")
                .args(["-TERM", &pid])
                .status()
                .await
                .expect("signal owned worker")
                .success()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(12), child.wait())
                .await
                .expect("shutdown deadline")
                .expect("exit")
                .success()
        );
        let rows:Vec<String>=sqlx::query_scalar("SELECT execution_profile_digest FROM worker_transcription_profile_observations WHERE node_id=$1")
            .bind(&d.node).fetch_all(d.store.pool()).await.expect("persisted facts");
        if offered {
            assert_eq!(rows, vec![reviewed.execution_profile_digest.clone()]);
        } else {
            assert!(rows.is_empty());
        }
    }
    server.stop().await;
    writer.drain().await.expect("private writer stopped");
}

#[tokio::test]
async fn populated_schema_50_keeps_its_live_worker_and_does_not_invent_observations() {
    let url = std::env::var("ORANGE_ASR_UPGRADE_DATABASE_URL").expect("dedicated older database");
    assert!(
        url.rsplit('/')
            .next()
            .is_some_and(|db| db.starts_with("orange_asr_old_"))
    );
    let store = Store::connect(&url).await.expect("actual PostgreSQL");
    let history = sqlx::migrate!();
    let old = sqlx::migrate::Migrator::with_migrations(
        history
            .iter()
            .filter(|m| m.version <= 50)
            .cloned()
            .collect(),
    );
    old.run(store.pool())
        .await
        .expect("apply original fifty migrations and checksums");
    let prior: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
        .fetch_one(store.pool())
        .await
        .expect("source version");
    assert_eq!(prior, 50);
    let d = device_in(store.clone(), "client-u").await;
    let profile = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest)
          VALUES($1,'historical-text',$2,$3)").bind(&profile).bind("0".repeat(64)).bind("1".repeat(64))
        .execute(store.pool()).await.expect("existing independently reviewed text profile");
    let legacy = historical_gateway::HistoricalGateway::start(50, &url, None).await;
    let mut rpc = gateway_client(legacy.port, Some(&d.certificate))
        .await
        .expect("pre-upgrade mTLS");
    let (sender, mut stream) = control(&mut rpc, hello(&d.node))
        .await
        .expect("existing protocol handshake");
    let before = welcome(&mut stream).await;
    heartbeat(&sender, &mut stream, &d.node, &before)
        .await
        .expect("old schema heartbeat");
    let before_migration:(i64,bool)=sqlx::query_as("SELECT max(version),to_regprocedure('lock_worker_issuer_policy()') IS NULL FROM _sqlx_migrations WHERE success").fetch_one(store.pool()).await.expect("actual historical schema");
    assert_eq!(
        before_migration,
        (50, true),
        "Historical peer must not silently migrate to current"
    );
    store.migrate().await.expect("upgrade live database");
    store.migrate().await.expect("idempotent upgrade");
    heartbeat(&sender, &mut stream, &d.node, &before)
        .await
        .expect("same certificate, session and epoch after upgrade");
    let retained: (String, i64) =
        sqlx::query_as("SELECT session_id,connection_epoch FROM worker_sessions WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(store.pool())
            .await
            .expect("unchanged live session");
    assert_eq!(
        retained,
        (before.session_id, before.connection_epoch as i64)
    );
    let observations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_transcription_profile_observations")
            .fetch_one(store.pool())
            .await
            .expect("observations");
    assert_eq!(observations, 0);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM reviewed_execution_profiles WHERE execution_profile_id=$1"
        )
        .bind(&profile)
        .fetch_one(store.pool())
        .await
        .expect("retained text profile")
    );
    drop(sender);
    drop(stream);
    drop(rpc);
    legacy.stop().await;
}
