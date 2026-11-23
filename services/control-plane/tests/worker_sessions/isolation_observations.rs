use super::*;
use orange_protocol::isolation;
use serde_json::{Value, json};
use tonic::Code;

const ACTUAL: &[u8] =
    include_bytes!("../../../../contracts/fixtures/isolation-v1/linux-x64-incomplete.json");

fn report(w: &worker::Welcome, now: i64) -> worker::IsolationObservation {
    worker::IsolationObservation {
        report_version: 1,
        session_id: w.session_id.clone(),
        connection_epoch: w.connection_epoch,
        request_id: Uuid::new_v4().to_string(),
        profile_id: isolation::PROFILE.into(),
        oci_policy_sha256: isolation::POLICY.into(),
        linux_probe_sha256: isolation::PROBE.into(),
        probe_json: ACTUAL.to_vec(),
        reported_capture_unix: now,
        node_policy_generation: Some(w.node_policy_generation),
    }
}
async fn now(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(pool)
        .await
        .expect("database clock")
}

#[tokio::test]
async fn private_observation_is_epoch_bound_bounded_and_never_grants_execution() {
    let d = device("client-isolation-first").await;
    let other = device("client-isolation-other").await;
    let pool = d.store.pool();
    let server = Running::start(&d.store).await;
    let mut raw = server
        .client(Some(&d.certificate))
        .await
        .expect("authenticated transport");
    let (link, mut stream) = control(&mut raw, hello(&d.node))
        .await
        .expect("real session");
    let active = welcome(&mut stream).await;
    let original = report(&active, now(pool).await);
    let mut foreign = server
        .client(Some(&other.certificate))
        .await
        .expect("other transport");
    assert_eq!(
        foreign
            .report_isolation_observation(original.clone())
            .await
            .expect_err("wrong peer")
            .code(),
        Code::Unauthenticated
    );
    let mut bad = original.clone();
    bad.connection_epoch += 1;
    assert_eq!(
        raw.report_isolation_observation(bad)
            .await
            .expect_err("wrong generation")
            .code(),
        Code::FailedPrecondition
    );
    let mut bad = original.clone();
    bad.oci_policy_sha256 = "d".repeat(64);
    assert_eq!(
        raw.report_isolation_observation(bad)
            .await
            .expect_err("unreviewed policy")
            .code(),
        Code::FailedPrecondition
    );
    let mut bad = original.clone();
    bad.reported_capture_unix -= 301;
    assert_eq!(
        raw.report_isolation_observation(bad)
            .await
            .expect_err("old submission")
            .code(),
        Code::FailedPrecondition
    );
    let mut bad = original.clone();
    let mut claims: Value = serde_json::from_slice(ACTUAL).expect("recorded sample");
    claims["supply_authorized"] = json!(true);
    bad.probe_json = serde_json::to_vec(&claims).expect("claims");
    assert_eq!(
        raw.report_isolation_observation(bad)
            .await
            .expect_err("invented supply")
            .code(),
        Code::InvalidArgument
    );
    let mut bad = original.clone();
    bad.probe_json = vec![b' '; 4097];
    assert_eq!(
        raw.report_isolation_observation(bad)
            .await
            .expect_err("oversized facts")
            .code(),
        Code::InvalidArgument
    );
    let mut sender = raw.clone();
    let (a, b) = tokio::join!(
        raw.report_isolation_observation(original.clone()),
        sender.report_isolation_observation(original.clone())
    );
    let a = a.expect("first report").into_inner();
    assert_eq!(a, b.expect("replayed report").into_inner());
    assert!(!a.supply_authorized && !a.wasm_enabled && a.disposition == "incomplete");
    let stored: (String, i64, String, Value) = sqlx::query_as(
        "SELECT certificate_sha256,connection_epoch,capabilities_digest,process_observation
        FROM worker_isolation_observations WHERE observation_id=$1::text::uuid",
    )
    .bind(&a.observation_id)
    .fetch_one(pool)
    .await
    .expect("durable evidence");
    assert_eq!(
        stored,
        (
            d.fingerprint.clone(),
            active.connection_epoch as i64,
            "a".repeat(64),
            serde_json::from_slice(ACTUAL).expect("original facts")
        )
    );
    let current = || {
        sqlx::query_scalar::<_, String>("SELECT observation_id::text FROM current_worker_isolation_observations WHERE node_id=$1 AND NOT supply_authorized AND NOT wasm_enabled")
        .bind(&d.node).fetch_optional(pool)
    };
    assert_eq!(
        current().await.expect("current receipt").as_deref(),
        Some(a.observation_id.as_str())
    );
    assert!(
        sqlx::query(
            "DELETE FROM worker_isolation_observations WHERE observation_id=$1::text::uuid"
        )
        .bind(&a.observation_id)
        .execute(pool)
        .await
        .is_err()
    );
    sqlx::query("INSERT INTO worker_isolation_observations(node_id,session_id,connection_epoch,node_policy_generation,request_id,certificate_sha256,
        capabilities_digest,profile_id,policy_digest,probe_digest,observation_digest,process_observation,disposition,reported_capture_unix,received_at)
        SELECT node_id,session_id,connection_epoch,node_policy_generation,'aged-test',certificate_sha256,capabilities_digest,profile_id,policy_digest,
        probe_digest,observation_digest,process_observation,disposition,reported_capture_unix-2678400,clock_timestamp()-interval '31 days'
        FROM worker_isolation_observations WHERE observation_id=$1::text::uuid").bind(&a.observation_id).execute(pool).await.expect("historical record");
    assert_eq!(
        orange_control_plane::worker::isolation_observations::expire(pool)
            .await
            .expect("delete aged diagnostic only"),
        1
    );
    sqlx::query("UPDATE nodes SET policy_generation=policy_generation+1 WHERE node_id=$1")
        .bind(&d.node)
        .execute(pool)
        .await
        .expect("new policy");
    assert!(current().await.expect("old policy invisible").is_none());
    assert_eq!(
        raw.report_isolation_observation(original.clone())
            .await
            .expect("replay historical receipt")
            .into_inner(),
        a
    );
    assert!(
        current()
            .await
            .expect("replay does not renew the policy")
            .is_none()
    );
    let mut stale = original.clone();
    stale.request_id = Uuid::new_v4().to_string();
    assert_eq!(
        raw.report_isolation_observation(stale.clone())
            .await
            .expect_err("old generation on fresh key")
            .code(),
        Code::FailedPrecondition
    );
    stale.node_policy_generation = None;
    assert_eq!(
        raw.report_isolation_observation(stale)
            .await
            .expect_err("omitted generation")
            .code(),
        Code::InvalidArgument
    );
    let refreshed = heartbeat(&link, &mut stream, &d.node, &active)
        .await
        .expect("authoritative generation");
    assert_eq!(
        refreshed.node_policy_generation,
        active.node_policy_generation + 1
    );
    let mut current_policy = active.clone();
    current_policy.node_policy_generation = refreshed.node_policy_generation;
    let mut complete = original.clone();
    complete.node_policy_generation = Some(refreshed.node_policy_generation);
    // A consistent but synthetic fully matching report still cannot enroll a GPU.
    for key in [
        "process_policy_match",
        "uid_gid_match",
        "no_new_privileges",
        "seccomp_filter",
        "capabilities_empty",
        "root_readonly",
        "private_tmp",
        "private_control",
        "unix_round_trip",
    ] {
        claims[key] = json!(true);
    }
    for key in [
        "windows_volume",
        "container_socket",
        "containerd_socket",
        "interop",
    ] {
        claims[key] = json!("absent");
    }
    claims["unexpected_mounts"] = json!(0);
    claims["nonloopback_interfaces"] = json!(0);
    claims["gpu_bridge"] = json!("character_device");
    claims["inet_socket"] = json!("permission_denied");
    claims["status"] = json!("observations_match");
    claims["supply_authorized"] = json!(false);
    claims["readonly_fixture"] = json!("readonly_filesystem");
    claims["descriptor_test"] = json!("process_limit_released");
    complete.probe_json = serde_json::to_vec(&claims).expect("synthetic complete report");
    assert_eq!(
        raw.report_isolation_observation(complete.clone())
            .await
            .expect_err("conflicting request")
            .code(),
        Code::AlreadyExists
    );
    complete.request_id = Uuid::new_v4().to_string();
    let checked = raw
        .report_isolation_observation(complete)
        .await
        .expect("claimed profile")
        .into_inner();
    assert_eq!(checked.disposition, "independent_review_required");
    assert!(!checked.supply_authorized && !checked.wasm_enabled);
    assert_eq!(
        current().await.expect("current policy").as_deref(),
        Some(checked.observation_id.as_str())
    );
    for _ in 2..24 {
        let r = report(&current_policy, now(pool).await);
        raw.report_isolation_observation(r)
            .await
            .expect("bounded unique report");
    }
    assert_eq!(
        raw.report_isolation_observation(report(&current_policy, now(pool).await))
            .await
            .expect_err("quota")
            .code(),
        Code::ResourceExhausted
    );
    assert_eq!(
        raw.report_isolation_observation(original.clone())
            .await
            .expect("old receipt while at quota")
            .into_inner(),
        a
    );
    assert_eq!(
        orange_control_plane::worker::isolation_observations::expire(pool)
            .await
            .expect("current window"),
        0
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_isolation_observations WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(pool)
            .await
            .expect("stored count");
    assert_eq!(rows, 24);
    let commands:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM admin_node_commands WHERE node_id=$1)+(SELECT count(*) FROM quote_execution_plans WHERE target_node_id=$1)").bind(&d.node).fetch_one(pool).await.expect("no side effect");
    assert_eq!(commands, 0);
    let mut next = server
        .client(Some(&d.certificate))
        .await
        .expect("reconnect");
    let (second, mut frames) = control(&mut next, hello(&d.node))
        .await
        .expect("new control");
    assert!(welcome(&mut frames).await.connection_epoch > active.connection_epoch);
    assert!(current().await.expect("old epoch invisible").is_none());
    assert!(matches!(
        raw.report_isolation_observation(original)
            .await
            .expect_err("old connection cannot replay")
            .code(),
        Code::Unauthenticated | Code::FailedPrecondition
    ));
    drop((link, stream, second, frames));
    server.stop().await;
}

#[tokio::test]
async fn isolated_report_waiting_on_the_node_lock_rolls_back_after_capture_expiry() {
    let d = device("client-isolation-wait").await;
    let pool = d.store.pool();
    let server = Running::start(&d.store).await;
    let mut raw = server.client(Some(&d.certificate)).await.expect("client");
    let (link, mut stream) = control(&mut raw, hello(&d.node)).await.expect("connect");
    let active = welcome(&mut stream).await;
    let mut held = pool.begin().await.expect("private barrier");
    let blocker: i32 =
        sqlx::query_scalar("SELECT pg_backend_pid() FROM nodes WHERE node_id=$1 FOR UPDATE")
            .bind(&d.node)
            .fetch_one(&mut *held)
            .await
            .expect("lock");
    let capture = now(pool).await - 296;
    let request = report(&active, capture);
    let release = async {
        tokio::time::timeout(Duration::from_secs(3),async {loop{
            let seen:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(blocker).fetch_one(pool).await.expect("wait observed");
            if seen {break;} tokio::time::sleep(Duration::from_millis(15)).await;
        }}).await.expect("peer queued");
        sqlx::query("SELECT pg_sleep_until(to_timestamp($1::bigint)+interval '20 milliseconds')")
            .bind(capture + 301)
            .execute(&mut *held)
            .await
            .expect("deadline");
        held.commit().await.expect("unlock");
    };
    let (answer, ()) = tokio::join!(raw.report_isolation_observation(request), release);
    assert_eq!(
        answer.expect_err("aged while waiting").code(),
        Code::FailedPrecondition
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_isolation_observations WHERE node_id=$1")
            .bind(&d.node)
            .fetch_one(pool)
            .await
            .expect("no receipt");
    assert_eq!(count, 0);
    drop((link, stream));
    server.stop().await;
}
