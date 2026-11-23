use super::*;
use std::time::Duration;

async fn grant(f: &Fixture, role: &str, kind: &str, scope: &str) {
    sqlx::query(
        "INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
        SELECT $1,$2,'dev',$3,$4,floor(extract(epoch FROM clock_timestamp()))::bigint+600",
    )
    .bind(&f.principal)
    .bind(role)
    .bind(kind)
    .bind(scope)
    .execute(&f.pool)
    .await
    .expect("explicit scoped grant");
}
async fn seed(
    pool: &PgPool,
    owner: &str,
    region: &str,
    issuer: &str,
    validity: i64,
    claimed: bool,
    withdrawn: bool,
    past_grant: bool,
) {
    let task = Uuid::new_v4().to_string();
    let node = Uuid::new_v4().to_string();
    let fingerprint = hash_secret(&task);
    let token = hash_secret(&node);
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
            .fetch_one(pool)
            .await
            .expect("clock");
    let created = if past_grant { now - 660 } else { now - 5 };
    sqlx::query("INSERT INTO worker_enrollment_grants(request_id,owner_id,requested_by,spki_sha256,csr_sha256,csr_der,exchange_token_sha256,created_at,expires_at)
        SELECT $1::text::uuid,$2,$2,$3,$3,decode(repeat('01',128),'hex'),$4,n,n+interval '10 minutes' FROM (SELECT to_timestamp($5::bigint)n)t")
        .bind(&task).bind(owner).bind(&fingerprint).bind(&token).bind(created).execute(pool).await.expect("approval metadata fixture");
    sqlx::query("INSERT INTO worker_enrollment_certificates(request_id,node_id,region_id,certificate_sha256,issuer_sha256,leaf_pem,not_before,expires_at,prepared_at,redemption_id,redeemed_at)
        SELECT $1::text::uuid,$2,$3,$4,$5,repeat('x',150),s,e,s+interval '1 second',CASE WHEN $8 THEN gen_random_uuid() END,CASE WHEN $8 THEN s+interval '2 seconds' END
        FROM (SELECT to_timestamp($6::bigint)s,to_timestamp($7::bigint)e)t").bind(&task).bind(&node).bind(region).bind(&fingerprint).bind(issuer)
        .bind(now-1200).bind(now+validity).bind(claimed).execute(pool).await.expect("historic leaf metadata fixture");
    if claimed {
        sqlx::query("INSERT INTO nodes(node_id,owner_id,environment,region_id,state) VALUES($1,$2,'dev',$3,'offline')")
            .bind(&node).bind(owner).bind(region).execute(pool).await.expect("node");
        sqlx::query("INSERT INTO worker_credentials(certificate_sha256,node_id,enrollment_user_id,not_before,expires_at)
            SELECT certificate_sha256,node_id,$2,not_before,expires_at FROM worker_enrollment_certificates WHERE request_id=$1::text::uuid")
            .bind(&task).bind(owner).execute(pool).await.expect("credential");
    }
    if withdrawn {
        sqlx::query("UPDATE worker_enrollment_grants SET state='revoked',revoked_at=clock_timestamp() WHERE request_id=$1::text::uuid")
        .bind(&task).execute(pool).await.expect("withdrawn credential");
    }
}
fn path(kind: &str, scope: &str) -> String {
    format!("/admin/v1/device-issuers?environment=dev&scope_kind={kind}&scope_id={scope}&limit=1")
}
#[tokio::test]
async fn issuer_directory_filters_regions_binds_positions_and_preserves_missing_historical_keys() {
    let f = fixture().await;
    let admin = fixture().await;
    let owner = Uuid::new_v4().to_string();
    let east = Uuid::new_v4().to_string();
    let west = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(&f.pool)
        .await
        .expect("owner");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'issuer-test',$1,$1)").bind(&owner).execute(&f.pool).await.expect("user");
    grant(&f, "security_operator", "region", &east).await;
    grant(&f, "finance_operator", "platform", "platform").await;
    grant(&admin, "auditor", "platform", "platform").await;
    let a = "1".repeat(64);
    let b = "2".repeat(64);
    let key = "8".repeat(64);
    for (r, i, valid, claimed, revoked, expired) in [
        (&east, &a, 500, true, false, false),
        (&east, &a, 3600, false, false, false),
        (&east, &a, -30, true, false, true),
        (&east, &a, 7200, true, true, false),
        (&west, &a, 7200, true, false, false),
        (&east, &b, 3600, false, false, true),
    ] {
        seed(&f.pool, &owner, r, i, valid, claimed, revoked, expired).await;
    }
    sqlx::query("INSERT INTO worker_issuer_public_keys(issuer_sha256,spki_sha256) VALUES($1,$2)")
        .bind(&a)
        .bind(&key)
        .execute(&f.pool)
        .await
        .expect("public key fixture");
    let e = path("region", &east);
    let global = path("platform", "platform");
    assert_eq!(get(&f, &global).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&f, &path("region", &west)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&f, &path("tenant", &f.a)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, page) = get(&f, &e).await;
    assert_eq!(status, StatusCode::OK);
    let row = &page["data"][0];
    assert_eq!(row["issuer_sha256"], a);
    assert_eq!(row["signing_key_sha256"], key);
    for (field, num) in [
        ("retained_certificates", "4"),
        ("awaiting_exchange", "1"),
        ("active_credentials", "1"),
        ("expiring_within_hour", "1"),
    ] {
        assert_eq!(row[field], num, "{field}");
    }
    assert!(
        row["next_expiry_unix"].is_string() && row.as_object().expect("bounded fields").len() == 8
    );
    let cursor = page["next_cursor"].as_str().expect("cursor");
    let second = get(&f, &format!("{e}&cursor={cursor}")).await;
    assert_eq!(second.0, StatusCode::OK);
    let old = &second.1["data"][0];
    assert_eq!(old["issuer_sha256"], b);
    assert!(
        old["signing_key_sha256"].is_null()
            && old["next_expiry_unix"].is_null()
            && second.1["next_cursor"].is_null()
    );
    assert_eq!(old["awaiting_exchange"], "0");
    assert_eq!(
        get(&admin, &format!("{global}&cursor={cursor}")).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (s, p) = get(&admin, &global).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["data"][0]["retained_certificates"], "5");
    assert_eq!(p["data"][0]["active_credentials"], "2");
    let audit_count:i64=sqlx::query_scalar("SELECT count(*) FROM admin_audit WHERE principal_id=$1 AND event_type='device_issuers.read' AND scope_id=$2")
        .bind(&f.principal).bind(&east).fetch_one(&f.pool).await.expect("scoped reads");
    assert_eq!(audit_count, 2);
    assert!(
        sqlx::query(
            "UPDATE worker_issuer_public_keys SET spki_sha256=repeat('9',64) WHERE issuer_sha256=$1"
        )
        .bind(a)
        .execute(&f.pool)
        .await
        .is_err()
    );
    sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND scope_id=$2")
        .bind(&f.principal)
        .bind(&east)
        .execute(&f.pool)
        .await
        .expect("revoke role");
    assert_eq!(
        get(&f, &format!("{e}&cursor={cursor}")).await.0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn issuer_read_does_not_publish_or_audit_after_role_expires_behind_a_lock() {
    let f = fixture().await;
    let east = Uuid::new_v4().to_string();
    let p = path("region", &east);
    grant(&f, "security_operator", "region", &east).await;
    let mut blocker = f.pool.begin().await.expect("audit barrier");
    sqlx::query("LOCK TABLE admin_audit IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .expect("lock audit");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .expect("pid");
    let until: i64 = sqlx::query_scalar(
        "UPDATE admin_grants SET expires_at=floor(extract(epoch FROM clock_timestamp()))::bigint+3
        WHERE principal_id=$1 AND scope_id=$2 RETURNING expires_at",
    )
    .bind(&f.principal)
    .bind(&east)
    .fetch_one(&f.pool)
    .await
    .expect("expiring role");
    let release = async {
        tokio::time::timeout(Duration::from_secs(2),async{loop{
            let seen:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(pid).fetch_one(&f.pool).await.expect("wait");
            if seen{break} tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.expect("audit insert waited");
        sqlx::query("SELECT pg_sleep_until(to_timestamp($1::bigint)+interval '30 milliseconds')")
            .bind(until)
            .execute(&mut *blocker)
            .await
            .expect("expire role");
        blocker.commit().await.expect("release");
    };
    let ((status, _), ()) = tokio::join!(get(&f, &p), release);
    assert_eq!(status, StatusCode::FORBIDDEN);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM admin_audit WHERE principal_id=$1 AND event_type='device_issuers.read'").bind(&f.principal)
        .fetch_one(&f.pool).await.expect("no successful read");
    assert_eq!(count, 0);
}
