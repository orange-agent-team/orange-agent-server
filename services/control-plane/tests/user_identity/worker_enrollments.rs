use super::*;
use sha2::{Digest, Sha256};

fn fixtures() -> Value {
    serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_TEST_ENROLLMENT_VECTORS").expect("public request fixtures"),
        )
        .expect("read"),
    )
    .expect("public JSON")
}
fn intent(f: &Fixture, id: &str) -> HeaderMap {
    let mut h = headers(f);
    h.insert("idempotency-key", id.parse().expect("id"));
    h
}
fn authorize_body(req: Value) -> Value {
    json!({"request":req,"exchange_token_sha256":hex::encode(Sha256::digest(random_secret().as_bytes()))})
}
fn endpoint(owner: &str, req: &Value) -> String {
    format!(
        "/api/v1/owners/{owner}/worker-enrollments/{}",
        req["request_id"].as_str().expect("request id")
    )
}
#[tokio::test]
async fn owner_approvals_verify_real_pkcs10_once_and_recheck_team_scope() {
    let f = fixture().await;
    let foreign = fixture().await;
    let vectors = fixtures();
    let req = &vectors["personal"];
    let id = req["request_id"].as_str().expect("id");
    let path = endpoint(&f.user, req);
    let body = authorize_body(req.clone());
    let mut csrf = intent(&f, id);
    csrf.remove("x-csrf-token");
    assert_eq!(
        request(&f, "PUT", &path, Some(body.clone()), csrf).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &foreign,
            "PUT",
            &path,
            Some(body.clone()),
            intent(&foreign, id)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for bad in vectors["malformed"]
        .as_array()
        .expect("negative CSR vectors")
    {
        let rejected = authorize_body(bad.clone());
        let (status, problem) = request(&f, "PUT", &path, Some(rejected), intent(&f, id)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["code"], "VALIDATION_ERROR");
    }
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_enrollment_grants WHERE owner_id=$1")
            .bind(&f.user)
            .fetch_one(&f.pool)
            .await
            .expect("no effects");
    assert_eq!(existing, 0);
    let a = request(&f, "PUT", &path, Some(body.clone()), intent(&f, id));
    let b = request(&f, "PUT", &path, Some(body.clone()), intent(&f, id));
    let (a, b) = tokio::join!(a, b);
    assert_eq!(a, b);
    assert_eq!(a.0, StatusCode::OK);
    let original = a.1;
    assert_eq!(original["state"], "authorized");
    assert_eq!(original["spki_sha256"], req["spki_sha256"]);
    assert!(original.get("exchange_token_sha256").is_none() && original.get("csr_pem").is_none());
    assert_eq!(
        request(&f, "GET", &path, None, headers(&f)).await,
        (StatusCode::OK, original.clone())
    );
    let different = authorize_body(req.clone());
    assert_eq!(
        request(&f, "PUT", &path, Some(different), intent(&f, id))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let revoked = request(&f, "DELETE", &path, None, intent(&f, id)).await;
    assert_eq!(revoked.0, StatusCode::OK);
    assert_eq!(revoked.1["state"], "revoked");
    assert_eq!(
        request(&f, "DELETE", &path, None, intent(&f, id)).await,
        revoked
    );
    assert_eq!(
        request(&f, "PUT", &path, Some(body), intent(&f, id)).await,
        revoked
    );
    let count:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM worker_enrollment_grants WHERE owner_id=$1),
        (SELECT count(*) FROM worker_credentials WHERE enrollment_user_id=$2),(SELECT count(*) FROM nodes WHERE owner_id=$1)")
        .bind(&f.user).bind(&f.user).fetch_one(&f.pool).await.expect("authority boundary");
    assert_eq!(count, (1, 0, 0));
    // Fill the rest of this owner's hourly allowance with isolated legacy rows.
    // The new key must fail without preventing a replay of the previously accepted receipt.
    sqlx::query("WITH t AS MATERIALIZED (SELECT clock_timestamp() AS n)
        INSERT INTO worker_enrollment_grants(request_id,owner_id,requested_by,spki_sha256,csr_sha256,exchange_token_sha256,csr_der,created_at,expires_at)
        SELECT gen_random_uuid(),g.owner_id,g.requested_by,g.spki_sha256,g.csr_sha256,
               encode(sha256(convert_to(gen_random_uuid()::text,'UTF8')),'hex'),g.csr_der,t.n,t.n+interval '10 minutes'
        FROM worker_enrollment_grants g CROSS JOIN t CROSS JOIN generate_series(1,23) WHERE g.request_id=$1::text::uuid")
        .bind(id).execute(&f.pool).await.expect("batch fixture");
    let quota = &vectors["quota"];
    assert_eq!(
        request(
            &f,
            "PUT",
            &endpoint(&f.user, quota),
            Some(authorize_body(quota.clone())),
            intent(&f, quota["request_id"].as_str().expect("id"))
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(request(&f, "GET", &path, None, headers(&f)).await, revoked);
    let org = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&org)
        .execute(&f.pool)
        .await
        .expect("owner");
    sqlx::query("INSERT INTO organizations(organization_id,name) VALUES($1,'Worker group')")
        .bind(&org)
        .execute(&f.pool)
        .await
        .expect("org");
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'admin'),($1,$3,'viewer')")
        .bind(&org).bind(&f.user).bind(&foreign.user).execute(&f.pool).await.expect("membership");
    let team = &vectors["team"];
    let t = team["request_id"].as_str().expect("id");
    let tp = endpoint(&org, team);
    let tb = authorize_body(team.clone());
    assert_eq!(
        request(&foreign, "PUT", &tp, Some(tb.clone()), intent(&foreign, t))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&f, "PUT", &tp, Some(tb.clone()), intent(&f, t))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE memberships SET role='member' WHERE user_id=$1 AND organization_id=$2")
        .bind(&f.user)
        .bind(&org)
        .execute(&f.pool)
        .await
        .expect("demote");
    for (method, body) in [("GET", None), ("DELETE", None), ("PUT", Some(tb))] {
        assert_eq!(
            request(&f, method, &tp, body, intent(&f, t)).await.0,
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn expired_worker_approval_keeps_original_deadline_and_aged_request_is_pruned() {
    let f = fixture().await;
    let req = fixtures()["expired"].clone();
    let id = req["request_id"].as_str().expect("id");
    let body = authorize_body(req.clone());
    let path = endpoint(&f.user, &req);
    let text = req["csr_pem"]
        .as_str()
        .expect("pem")
        .lines()
        .filter(|l| !l.starts_with('-'))
        .collect::<String>();
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("DER");
    for (id, age, secret) in [
        (
            id.to_string(),
            "11 minutes",
            body["exchange_token_sha256"]
                .as_str()
                .expect("hash")
                .to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            "31 days",
            hex::encode(Sha256::digest(random_secret().as_bytes())),
        ),
    ] {
        sqlx::query("WITH t AS MATERIALIZED(SELECT clock_timestamp()-$1::text::interval AS n) INSERT INTO worker_enrollment_grants
            (request_id,owner_id,requested_by,spki_sha256,csr_sha256,exchange_token_sha256,csr_der,created_at,expires_at)
            SELECT $2::text::uuid,$3,$3,$4,$5,$6,$7,n,n+interval '10 minutes' FROM t").bind(age).bind(id).bind(&f.user)
            .bind(req["spki_sha256"].as_str()).bind(req["csr_sha256"].as_str()).bind(secret).bind(&der)
            .execute(&f.pool).await.expect("historical record");
    }
    let old = request(&f, "GET", &path, None, headers(&f)).await;
    assert_eq!(old.0, StatusCode::OK);
    assert_eq!(old.1["state"], "expired");
    assert_eq!(
        request(&f, "PUT", &path, Some(body), intent(&f, id)).await,
        old
    );
    assert_eq!(
        orange_control_plane::auth::routes::prune_worker_enrollment_grants(&f.pool)
            .await
            .expect("aged cleanup"),
        1
    );
    assert_eq!(request(&f, "GET", &path, None, headers(&f)).await, old);
}
