//! Actual browser plus owned API/Worker/Counter. Session, price and enrollment
//! remain explicit development fixtures; no production identity/payment claim.
use super::*;
#[path = "cloud_model_browser.rs"]
mod hosted;
#[tokio::test]
async fn actual_browser_quote_model_settlement_cleanup_and_deleted_content() {
    use super::super::host_cases as host;
    use std::{os::unix::fs::PermissionsExt, process::Stdio};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
    };
    let c = setup_priced("client-dk").await;
    let p = profile();
    let worker_root = runtime();
    let meter_root = runtime();
    let worker_config = host::configuration_port(&c.f, 1, &p, worker_root.path());
    host::enable_outputs(&worker_config);
    let mut worker_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&worker_config).expect("config")).expect("JSON");
    worker_value["confirm_cleanup"] = serde_json::json!(true);
    std::fs::write(
        &worker_config,
        serde_json::to_vec(&worker_value).expect("JSON"),
    )
    .expect("private config");
    let config_root = runtime();
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS"));
    let gateway_config = config_root.path().join("gateway.json");
    let meter_config = config_root.path().join("meter.json");
    for (path, value) in [
        (
            &gateway_config,
            serde_json::json!({"gateway_instance_id":Uuid::new_v4().to_string(),"server_certificate_file":tls.join("server.pem"),"server_key_file":tls.join("server.key"),"client_ca_file":tls.join("ca.pem")}),
        ),
        (
            &meter_config,
            serde_json::json!({"protocol":1,"runtime_directory":meter_root.path(),"executable":p.executable,"executable_sha256":p.executable_sha256,"model":p.model,"model_sha256":p.model_sha256,"model_bytes":p.model_bytes,"model_version":p.model_version,"context_tokens":p.context_tokens}),
        ),
    ] {
        std::fs::write(path, serde_json::to_vec(&value).expect("JSON")).expect("private config");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("private");
    }
    let identity_config = config_root.path().join("user.json");
    let mut identity: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity config"))
            .expect("private identity config"),
    )
    .expect("JSON");
    let port = std::env::var("ORANGE_WEB_COMPUTE_QA_PORT").unwrap_or_else(|_| "43127".into());
    assert!(matches!(port.as_str(), "43127" | "43128"));
    let user_origin = format!("https://127.0.0.1:{port}");
    identity["user_origin"] = serde_json::json!(user_origin);
    std::fs::write(
        &identity_config,
        serde_json::to_vec(&identity).expect("JSON"),
    )
    .expect("private identity clone");
    std::fs::set_permissions(&identity_config, std::fs::Permissions::from_mode(0o600))
        .expect("private");
    c.f.objects.drain().await.expect("old writer drain");
    drop(c.f.objects);
    c.meter.shutdown().await.expect("old counter stopped");
    let mut connector_env = std::collections::BTreeMap::new();
    if let Some(path) = std::env::var_os("ORANGE_MODEL_CONNECTOR_FIXTURE") {
        let mut source: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("private connector config"))
                .expect("connector fixture");
        source["owner_id"] = serde_json::json!(c.f.owner);
        let file = config_root.path().join("s3-registration.json");
        std::fs::write(&file, serde_json::to_vec(&[source]).expect("source"))
            .expect("private source config");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("private");
        connector_env.insert("ORANGE_S3_SOURCES", file);
    }
    let mut api = Command::new(std::env::var("ORANGE_USER_API_BINARY").expect("actual API"))
        .env_clear()
        .envs(connector_env)
        .env("ORANGE_ENV", "development")
        .env(
            "DATABASE_URL",
            std::env::var("ORANGE_TEST_DATABASE_URL").expect("DB"),
        )
        .env("ORANGE_USER_CONFIG_FILE", &identity_config)
        .env("ORANGE_USER_BIND", "127.0.0.1:0")
        .env("ORANGE_ARTIFACT_ROOT", &c.f.root)
        .env("ORANGE_ARTIFACT_BYTE_LIMIT", "1048576")
        .env("ORANGE_ARTIFACT_RECORD_LIMIT", "100")
        .env("ORANGE_WORKER_GATEWAY_CONFIG", &gateway_config)
        .env("ORANGE_WORKER_GATEWAY_BIND", "127.0.0.1:0")
        .env("ORANGE_METER_CONFIG", &meter_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("API/counter process");
    let mut logs = BufReader::new(api.stderr.take().expect("diagnostics")).lines();
    let (http, grpc) = tokio::time::timeout(Duration::from_secs(35), async {
        let mut http = None;
        let mut grpc = None;
        loop {
            let line = logs.next_line().await.expect("read").expect("live API");
            if let Some(v) = line.strip_prefix("Development User API listening on ") {
                http = Some(
                    v.split(';')
                        .next()
                        .expect("address")
                        .parse::<std::net::SocketAddr>()
                        .expect("address"),
                );
            }
            if let Some(v) = line.strip_prefix("Development output Gateway listening on ") {
                grpc = Some(v.parse::<std::net::SocketAddr>().expect("address"));
            }
            if let (Some(h), Some(g)) = (http, grpc) {
                break (h, g);
            }
        }
    })
    .await
    .expect("actual listeners");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&worker_config).expect("worker config"))
            .expect("JSON");
    value["endpoint"] = serde_json::json!(format!("https://{grpc}/"));
    std::fs::write(&worker_config, serde_json::to_vec(&value).expect("JSON")).expect("endpoint");
    let (mut worker, mut events) = host::start(&worker_config);
    host::event(&mut events, "ready").await;

    let fixture = config_root.path().join("browser.json");
    std::fs::write(&fixture,serde_json::to_vec(&serde_json::json!({"api":format!("http://{http}/"),"origin":user_origin,"cookie":c.f.headers.get("cookie").expect("cookie fixture").to_str().expect("cookie"),"user":c.f.user,"owner":c.f.owner,"budget":c.budget,"price":c.price})).expect("JSON")).expect("fixture");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o600)).expect("private");
    let mut browser = Command::new(std::env::var("ORANGE_WEB_NODE_BINARY").expect("pinned Node"))
        .arg(std::env::var("ORANGE_WEB_COMPUTE_DRIVER").expect("explicit Web driver"))
        .arg(&fixture)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("owned actual browser driver");
    let status = tokio::time::timeout(Duration::from_secs(120), browser.wait())
        .await
        .expect("browser bound")
        .expect("browser exit");
    let evidence = PathBuf::from(
        std::env::var("ORANGE_WEB_COMPUTE_EVIDENCE_DIR").expect("private evidence directory"),
    );
    for name in [
        "browser-error.txt",
        "browser-failed.png",
        "browser-model-result.png",
        "browser-result.json",
        "browser-input.json",
        "browser-output.json",
    ] {
        let file = config_root.path().join(name);
        if file.is_file() {
            std::fs::copy(file, evidence.join(name))
                .expect("preserve generated-test evidence only");
        }
    }
    if !status.success() {
        let mut diagnostic = String::new();
        tokio::io::AsyncReadExt::read_to_string(
            &mut browser.stderr.take().expect("driver diagnostic"),
            &mut diagnostic,
        )
        .await
        .expect("read diagnostic");
        eprintln!("Actual browser driver failed: {diagnostic}");
        if worker.try_wait().expect("owned worker").is_none() {
            host::terminate(&mut worker).await;
        }
        if api.try_wait().expect("owned API").is_none() {
            let pid = rustix::process::Pid::from_raw(api.id().expect("owned unreaped API") as i32)
                .expect("pid");
            rustix::process::kill_process(pid, rustix::process::Signal::INT)
                .expect("owned API stop");
            match tokio::time::timeout(Duration::from_secs(20), api.wait()).await {
                Ok(Ok(status)) if status.success() => {}
                result => eprintln!(
                    "Owned API cleanup was not confirmed after browser failure: {result:?}"
                ),
            }
        }
        panic!(
            "Actual browser acceptance failed; original diagnostic and private screenshot retained"
        );
    }
    let observation: serde_json::Value = serde_json::from_slice(
        &std::fs::read(config_root.path().join("browser-result.json"))
            .expect("observed browser result"),
    )
    .expect("JSON");
    let job = observation["job_id"].as_str().expect("job");
    assert_eq!(observation["content_denied_after_deletion"], true);
    assert_eq!(observation["late_withdrawal_preserved"], true);
    let prevented = observation["prevented_invocation"]
        .as_str()
        .expect("prevented original Invocation");
    let prevented_jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE owner_id=$1 AND invocation_id=$2")
            .bind(&c.f.owner)
            .bind(prevented)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("no accepted stopped Job");
    assert_eq!(prevented_jobs, 0);
    let barrier: String = sqlx::query_scalar(
        "SELECT receipt_id FROM invocation_withdrawals WHERE owner_id=$1 AND user_id=$2 AND invocation_id=$3",
    )
    .bind(&c.f.owner)
    .bind(&c.f.user)
    .bind(prevented)
    .fetch_one(c.f.d.store.pool())
    .await
    .expect("actual prevention receipt");
    assert_eq!(observation["withdrawal_receipt"], barrier);
    host::event(&mut events, "cloud_cleanup_confirmed").await;
    let result = c.f.d.store.job(&c.f.owner, job).await.expect("actual Job");
    assert_eq!(result.execution_state, "succeeded");
    assert_eq!(result.settlement_state, "settled");
    assert!(result.charge.expect("actual charge").0.units() > 0);
    let phase: String =
        sqlx::query_scalar("SELECT state FROM worker_execution_attempts WHERE job_id=$1")
            .bind(job)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("resource");
    assert_eq!(phase, "closed");
    let input: orange_protocol::model::GenerateRequest = serde_json::from_slice(
        &std::fs::read(config_root.path().join("browser-input.json"))
            .expect("actual previously authorized fixture bytes"),
    )
    .expect("input");
    let bytes = std::fs::read(config_root.path().join("browser-output.json"))
        .expect("actual committed fixture bytes");
    let output: orange_protocol::model::ResponseCandidate =
        serde_json::from_slice(&bytes).expect("output");
    assert_eq!(input.invocation_id, result.invocation_id);
    assert_eq!(input.max_output_tokens, 32);
    assert!(!output.text.trim().is_empty());
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        observation["output_sha256"].as_str().expect("hash")
    );
    let oracle = TrustedMeter::launch(c._meter_root.path(), profile())
        .await
        .expect("independent actual oracle");
    let expected_input = oracle.input(input.clone()).await.expect("input units");
    let expected_output = oracle
        .output(input, output, 32)
        .await
        .expect("output units");
    oracle.shutdown().await.expect("oracle exit");
    let units: (i64, i64) =
        sqlx::query_as("SELECT input_units,output_units FROM job_usage_facts WHERE job_id=$1")
            .bind(job)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("usage facts");
    assert_eq!(
        units,
        (
            expected_input.units() as i64,
            expected_output.units() as i64
        )
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_settlements WHERE job_id=$1")
        .bind(job)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("one settlement");
    assert_eq!(count, 1);
    let balance:i64=sqlx::query_scalar("SELECT sum(e.amount)::bigint FROM ledger_entries e JOIN job_settlements s USING(transaction_id) WHERE s.job_id=$1").bind(job).fetch_one(c.f.d.store.pool()).await.expect("balanced ledger");
    assert_eq!(balance, 0);
    if let Ok(driver) = std::env::var("ORANGE_CLOUD_MODEL_BROWSER") {
        let ctx = hosted::Setup {
            pool: c.f.d.store.pool(),
            object_root: &c.f.root,
            api: http,
            user: &c.f.user,
            owner: &c.f.owner,
            budget: &c.budget,
            price: &c.price,
            cookie: c
                .f
                .headers
                .get("cookie")
                .expect("private test session")
                .to_str()
                .expect("fixture cookie"),
        };
        hosted::exercise(ctx, &driver).await;
        host::event(&mut events, "cloud_cleanup_confirmed").await;
    }
    host::terminate(&mut worker).await;
    host::stopped(worker_root.path());
    assert!(api.try_wait().expect("owned API").is_none());
    let pid =
        rustix::process::Pid::from_raw(api.id().expect("owned unreaped API") as i32).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::INT).expect("owned shutdown");
    assert!(
        tokio::time::timeout(Duration::from_secs(20), api.wait())
            .await
            .expect("API drain")
            .expect("API exit")
            .success()
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(meter_root.path().join("process.json")).expect("counter stop receipt"),
    )
    .expect("JSON");
    assert_eq!(state["phase"], "stopped");
}
