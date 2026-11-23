//! Exact schema-78 upgrade, conflicting legacy credentials, and concurrent purpose reservations.
use orange_control_plane::store::Store;
use sqlx::{PgPool, Postgres, Transaction};
use std::{path::Path, time::Duration};
use uuid::Uuid;

const WORKER: &str = "INSERT INTO worker_credentials(certificate_sha256,node_id,enrollment_user_id,not_before,expires_at)
    VALUES($1,'purpose-node','purpose-user',clock_timestamp()-interval '1 minute',clock_timestamp()+interval '1 hour')";
const HOST: &str = "INSERT INTO cloud_pool_hosts(host_instance_id,certificate_sha256,profile_id,attestation_digest,region_id,max_workspaces,
    credential_expires_at,accepting,live_until) VALUES($2,$1,'dev-gvisor-linux-arm64-v1',repeat('a',64),'dev',1,
    clock_timestamp()+interval '1 hour',true,clock_timestamp()+interval '1 minute')";

async fn below_79(pool: &PgPool) {
    let folder = tempfile::tempdir().expect("schema 78");
    let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for file in std::fs::read_dir(migrations).expect("versions") {
        let file = file.expect("file");
        let name = file.file_name();
        let (prefix, _) = name
            .to_str()
            .expect("name")
            .split_once('_')
            .expect("version");
        if prefix.parse::<u32>().expect("numeric") < 79 {
            std::fs::copy(file.path(), folder.path().join(name)).expect("legacy SQL");
        }
    }
    sqlx::migrate::Migrator::new(folder.path())
        .await
        .expect("legacy migrator")
        .run(pool)
        .await
        .expect("migrate to 78");
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await
            .expect("version");
    assert_eq!(version, 78);
}
fn check_denied(error: sqlx::Error) {
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23514")
    );
}
async fn insert_host(tx: &mut Transaction<'_, Postgres>, digest: &str) -> Result<(), sqlx::Error> {
    sqlx::query(HOST)
        .bind(digest)
        .bind(Uuid::new_v4().to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[tokio::test]
async fn upgrade_and_concurrent_enrollment_bind_one_peer_role_per_certificate() {
    let store =
        Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("private test DB"))
            .await
            .expect("store");
    let pool = store.pool();
    below_79(pool).await;
    sqlx::raw_sql("INSERT INTO owners(owner_id) VALUES('purpose-owner');
        INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('purpose-user','test','purpose-user','purpose-owner');
        INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES('purpose-node','purpose-owner','dev','local','online');")
        .execute(pool).await.expect("legacy identity fixture");
    let collision = "a".repeat(64);
    let worker_only = "b".repeat(64);
    let host_only = "c".repeat(64);
    for fingerprint in [&worker_only, &collision] {
        sqlx::query(WORKER)
            .bind(fingerprint)
            .execute(pool)
            .await
            .expect("legacy device registration");
    }
    for (fingerprint, subject) in [(&host_only, "kept"), (&collision, "contested")] {
        sqlx::query(HOST)
            .bind(fingerprint)
            .bind(subject)
            .execute(pool)
            .await
            .expect("legacy host");
    }
    sqlx::query("INSERT INTO worker_sessions(node_id,gateway_instance_id,connection_epoch,session_id,certificate_sha256,capabilities_digest,last_seen_at,expires_at)
        VALUES('purpose-node','test',1,'legacy',$1,repeat('f',64),clock_timestamp(),clock_timestamp()+interval '1 hour')")
        .bind(&collision).execute(pool).await.expect("live legacy stream");
    store.migrate().await.expect("upgrade to current schema");
    let protected:(bool,bool,bool,bool,String)=sqlx::query_as("SELECT NOT c.active,NOT h.enabled AND NOT h.accepting,h.live_until IS NULL,
        s.expires_at<=clock_timestamp(),p.purpose FROM execution_certificate_purposes p JOIN worker_credentials c USING(certificate_sha256)
        JOIN worker_sessions s USING(certificate_sha256) JOIN cloud_pool_hosts h USING(certificate_sha256) WHERE p.certificate_sha256=$1")
        .bind(&collision).fetch_one(pool).await.expect("legacy overlap");
    assert_eq!(
        protected,
        (true, true, true, true, "retired_conflict".into())
    );
    let intact: bool = sqlx::query_scalar(
        "SELECT (SELECT active FROM worker_credentials WHERE certificate_sha256=$1)
        AND (SELECT enabled AND accepting FROM cloud_pool_hosts WHERE certificate_sha256=$2)",
    )
    .bind(&worker_only)
    .bind(&host_only)
    .fetch_one(pool)
    .await
    .expect("unrelated active peers");
    assert!(intact);
    for change in [
        "UPDATE worker_credentials SET active=true WHERE certificate_sha256=$1",
        "UPDATE cloud_pool_hosts SET enabled=true WHERE certificate_sha256=$1",
        "UPDATE execution_certificate_purposes SET purpose='platform_host',subject_id='contested' WHERE certificate_sha256=$1",
        "DELETE FROM execution_certificate_purposes WHERE certificate_sha256=$1",
    ] {
        check_denied(
            sqlx::query(change)
                .bind(&collision)
                .execute(pool)
                .await
                .expect_err("quarantine is durable"),
        );
    }
    check_denied(
        sqlx::query(HOST)
            .bind(&worker_only)
            .bind("foreign-worker")
            .execute(pool)
            .await
            .expect_err("worker leaf at managed pool"),
    );
    check_denied(
        sqlx::query(WORKER)
            .bind(&host_only)
            .execute(pool)
            .await
            .expect_err("managed leaf at public worker"),
    );
    let protected_host = sqlx::query("DELETE FROM cloud_pool_hosts WHERE certificate_sha256=$1")
        .bind(&host_only)
        .execute(pool)
        .await
        .expect_err("existing platform identity guard");
    assert!(
        protected_host
            .as_database_error()
            .is_some_and(|e| e.message() == "retired platform host identity cannot be reused")
    );
    sqlx::query("DELETE FROM worker_credentials WHERE certificate_sha256=$1")
        .bind(&worker_only)
        .execute(pool)
        .await
        .expect("remove an unused public worker credential");
    check_denied(
        sqlx::query(HOST)
            .bind(&worker_only)
            .bind("former-worker")
            .execute(pool)
            .await
            .expect_err("removed leaf retains purpose"),
    );
    let fresh = "d".repeat(64);
    let mut first = pool.begin().await.expect("owner");
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *first)
        .await
        .expect("owner PID");
    sqlx::query(WORKER)
        .bind(&fresh)
        .execute(&mut *first)
        .await
        .expect("tentative worker claim");
    let mut second = pool.begin().await.expect("contender");
    let release = async {
        tokio::time::timeout(Duration::from_secs(4),async {loop{
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(blocker).fetch_one(pool).await.expect("wait observation");
            if waiting {break} tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.expect("other registrar actually waited");
        first.commit().await.expect("winner committed");
    };
    let (loser, ()) = tokio::join!(insert_host(&mut second, &fresh), release);
    check_denied(loser.expect_err("one winner"));
    second.rollback().await.expect("loser rollback");
    let one: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM worker_credentials WHERE certificate_sha256=$1),
            (SELECT count(*) FROM cloud_pool_hosts WHERE certificate_sha256=$1)",
    )
    .bind(&fresh)
    .fetch_one(pool)
    .await
    .expect("one durable registration");
    assert_eq!(one, (1, 0));
    let abandoned = "e".repeat(64);
    let mut tx = pool.begin().await.expect("abandoned registration");
    insert_host(&mut tx, &abandoned)
        .await
        .expect("tentative host");
    tx.rollback().await.expect("rollback");
    sqlx::query(WORKER)
        .bind(&abandoned)
        .execute(pool)
        .await
        .expect("original transaction didn't claim purpose");
}
