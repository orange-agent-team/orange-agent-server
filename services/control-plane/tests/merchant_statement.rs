//! Real PostgreSQL, sandbox HTTP transport and account-scoped Admin reads. Cash lines are test fixtures.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use orange_control_plane::{
    admin::{
        hash_secret,
        identity::{AdminIdentity, OidcConfig},
        random_secret, routes,
    },
    auth::users::{UserIdentity, UserOidcConfig},
    payments::{Payments, Sandbox, SandboxConfig},
    store::Store,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;
const MERCHANT: &str = "acct_statement_fixture";
struct Remote {
    start: i64,
    phase: AtomicUsize,
    arrived: Notify,
    resume: Notify,
}
fn line(id: &str, source: &str, created: i64, currency: &str, amount: i64, fee: i64) -> Value {
    json!({"object":"balance_transaction","id":id,"source":source,"created":created,"currency":currency,"amount":amount,"fee":fee,"net":amount-fee,"type":"charge","reporting_category":"charge","status":"available"})
}
fn authenticated(h: &HeaderMap) {
    assert_eq!(h["stripe-version"], "2026-11-18.endive");
    assert!(h.contains_key("authorization"));
}
async fn account(h: HeaderMap) -> Json<Value> {
    authenticated(&h);
    Json(json!({"object":"account","id":MERCHANT}))
}
async fn page(
    State(r): State<Arc<Remote>>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> axum::response::Response {
    authenticated(&h);
    assert_eq!(q["created[gte]"], r.start.to_string());
    assert_eq!(q["limit"], "100");
    let overlap = q["created[lt]"] == (r.start + 172800).to_string();
    assert!(overlap || q["created[lt]"] == (r.start + 86400).to_string());
    let t = r.start + 100;
    let after = q.get("starting_after").map(String::as_str);
    let phase = r.phase.load(Ordering::SeqCst);
    if phase == 1 {
        r.arrived.notify_one();
        r.resume.notified().await;
    }
    if after == Some("txn_last_a") && phase == 2 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let (data, more) = if let Some(id) = after {
        assert_eq!(id, "txn_last_a");
        (
            vec![
                line("txn_fx", "ch_fx", t, "eur", 200, 6),
                line("txn_diff", "ch_diff", t, "jpy", 775, 25),
                line("txn_dupe_a", "ch_dupe", t, "usd", 100, 2),
                line("txn_dupe_b", "ch_dupe", t, "usd", 100, 2),
                {
                    let mut refund = line("txn_refund", "re_unposted", t, "usd", -100, 0);
                    refund["type"] = json!("refund");
                    refund["reporting_category"] = json!("refund");
                    refund
                },
                line("txn_foreign", "ch_foreign", t, "usd", 500, 5),
            ],
            false,
        )
    } else {
        let mut v = vec![];
        if overlap {
            v.push(line("txn_today", "ch_later", t + 86400, "hkd", 350, 11));
        }
        v.push(line(
            "txn_last_a",
            "ch_good",
            t,
            "usd",
            500,
            if overlap { 13 } else { 12 },
        ));
        (v, true)
    };
    Json(json!({"object":"list","data":data,"has_more":more})).into_response()
}
async fn seed(db: &Store, owner: &str, first: i64, id: &str, currency: &str, minor: i64) {
    let order = Uuid::new_v4().to_string();
    let compact = order.replace('-', "");
    let txid = Uuid::new_v4().to_string();
    let mut t = db.pool().begin().await.expect("fixture transaction");
    sqlx::query("INSERT INTO test_credit_offers VALUES($1,$2,$3,$4,$5,1000,true)")
        .bind(MERCHANT)
        .bind(&order)
        .bind(format!("price_{compact}"))
        .bind(currency)
        .bind(minor)
        .execute(&mut *t)
        .await
        .expect("offer");
    sqlx::query("INSERT INTO test_checkout_orders(order_id,owner_id,requester_id,merchant_id,offer_id,terms_digest,state,dispatch_binding,session_id,dispatched_at,created_at)
        VALUES($1,$2,$2,$3,$1,$4,'credited',$5,$6,to_timestamp($7::bigint),to_timestamp($7::bigint))")
        .bind(&order).bind(owner).bind(MERCHANT).bind("a".repeat(64)).bind(&compact).bind(format!("cs_test_{compact}")).bind(first).execute(&mut *t).await.expect("order");
    sqlx::query("INSERT INTO ledger_transactions(transaction_id,business_key,owner_id,created_at) VALUES($1,$2,$3,to_timestamp($4::bigint))")
        .bind(&txid).bind(format!("test-checkout:{order}")).bind(owner).bind(first).execute(&mut *t).await.expect("transaction");
    sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,$2,1000),($1,$3,-1000)")
        .bind(&txid).bind(format!("{owner}:purchased:available")).bind(format!("stripe-test:{MERCHANT}:clearing")).execute(&mut *t).await.expect("balanced original entries");
    sqlx::query("UPDATE wallets SET purchased=purchased+1000 WHERE owner_id=$1")
        .bind(owner)
        .execute(&mut *t)
        .await
        .expect("prior customer credit");
    sqlx::query("INSERT INTO test_credit_fulfillments(order_id,session_id,payment_intent,charge_id,owner_id,amount_minor,currency,microcredits,transaction_id,recorded_at)
        VALUES($1,$2,$3,$4,$5,$6,$7,1000,$8,to_timestamp($9::bigint))").bind(&order).bind(format!("cs_test_{compact}")).bind(format!("pi_{compact}")).bind(id)
        .bind(owner).bind(minor).bind(currency).bind(txid).bind(first).execute(&mut *t).await.expect("existing confirmed purchase");
    t.commit().await.expect("historic facts");
}
async fn proof(db: &Store, owner: &str) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT w.purchased,w.reserved_purchased,(SELECT count(*) FROM ledger_transactions WHERE owner_id=$1),(SELECT count(*) FROM test_credit_fulfillments WHERE owner_id=$1) FROM wallets w WHERE owner_id=$1")
        .bind(owner).fetch_one(db.pool()).await.expect("customer accounting")
}
async fn operator(db: &Store, issuer: &str, kind: &str, scope: &str) -> String {
    let principal = Uuid::new_v4().to_string();
    let token = random_secret();
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&principal)
        .bind(issuer)
        .execute(db.pool())
        .await
        .expect("test operator");
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'finance_operator','dev',$2,$3,floor(extract(epoch FROM clock_timestamp()))::bigint+600)").bind(&principal).bind(kind).bind(scope).execute(db.pool()).await.expect("scoped role");
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) SELECT $1,$2,$3,n,n+600,n+900 FROM (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint n) t")
        .bind(hash_secret(&token)).bind(principal).bind(random_secret()).execute(db.pool()).await.expect("explicit development session");
    token
}
async fn read(router: &Router, token: &str, path: &str) -> (u16, Value) {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("cookie", format!("__Host-orange-admin={token}"))
                .header("x-request-id", "statement-test")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    let status = res.status().as_u16();
    let data = to_bytes(res.into_body(), 65536)
        .await
        .expect("bounded report");
    (status, serde_json::from_slice(&data).expect("JSON receipt"))
}
#[tokio::test]
async fn bounded_merchant_scan_survives_lost_claim_partial_failure_and_overlapping_days() {
    let db = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("owned PostgreSQL"))
        .await
        .expect("PG");
    db.migrate().await.expect("migrate");
    let cfg: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("user config"))
            .expect("config"),
    )
    .expect("user OIDC");
    let identity = UserIdentity::discover(db.pool().clone(), cfg)
        .await
        .expect("real development discovery");
    let owner = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners VALUES($1,'active')")
        .bind(&owner)
        .execute(db.pool())
        .await
        .expect("owner");
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(db.pool())
        .await
        .expect("wallet");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'statement-test',$1,$1)").bind(&owner).execute(db.pool()).await.expect("buyer");
    let start: i64 = sqlx::query_scalar(
        "SELECT (floor(extract(epoch FROM clock_timestamp())/86400)::bigint-2)*86400",
    )
    .fetch_one(db.pool())
    .await
    .expect("UTC day");
    for (id, currency, value) in [
        ("ch_good", "usd", 500),
        ("ch_missing", "usd", 400),
        ("ch_diff", "jpy", 800),
        ("ch_fx", "hkd", 250),
        ("ch_dupe", "usd", 100),
    ] {
        seed(&db, &owner, start + 10, id, currency, value).await;
    }
    // A merchant refund landed while local finance was still holding the original units.
    let source: String = sqlx::query_scalar(
        "SELECT order_id FROM test_credit_fulfillments WHERE charge_id='ch_good'",
    )
    .fetch_one(db.pool())
    .await
    .expect("purchase");
    let case = Uuid::new_v4().to_string();
    let rid = Uuid::new_v4().to_string();
    let mut hold = db.pool().begin().await.expect("historical pending hold");
    sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at) VALUES($1,$2,'dev',$3,'open',$4)").bind(&case).bind(&owner).bind(format!("test-refund:{rid}")).bind(start).execute(&mut *hold).await.expect("refund case");
    sqlx::query("INSERT INTO test_refund_orders(refund_id,order_id,owner_id,requester_id,case_id,currency,amount_minor,held_units,prior_minor,target_version,state,dispatch_binding,provider_refund_id,created_at) VALUES($1,$2,$3,$3,$4,'usd',100,200,0,$5,'pending',$6,'re_unposted',to_timestamp($7::bigint))")
        .bind(&rid).bind(&source).bind(&owner).bind(&case).bind("b".repeat(64)).bind(Uuid::new_v4().simple().to_string()).bind(start+50).execute(&mut *hold).await.expect("pending external refund");
    sqlx::query("UPDATE wallets SET purchased=purchased-200,reserved_purchased=reserved_purchased+200 WHERE owner_id=$1").bind(&owner).execute(&mut *hold).await.expect("existing wallet hold");
    sqlx::query("UPDATE test_purchase_lots SET available=available-200,refund_held=refund_held+200 WHERE order_id=$1").bind(&source).execute(&mut *hold).await.expect("existing source hold");
    let tid = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&tid)
    .bind(format!("test-refund-hold:{rid}"))
    .bind(&owner)
    .execute(&mut *hold)
    .await
    .expect("original hold transaction");
    sqlx::query("INSERT INTO ledger_entries VALUES($1,$2,-200),($1,$3,200)")
        .bind(tid)
        .bind(format!("{owner}:purchased:available"))
        .bind(format!("{owner}:purchased:refund-held"))
        .execute(&mut *hold)
        .await
        .expect("balanced hold");
    hold.commit().await.expect("prior committed state");
    let before = proof(&db, &owner).await;
    let r = Arc::new(Remote {
        start,
        phase: AtomicUsize::new(1),
        arrived: Notify::new(),
        resume: Notify::new(),
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("merchant listener");
    let addr = socket.local_addr().expect("loopback");
    let transport = Sandbox::loopback_fixture(
        SandboxConfig {
            merchant_id: MERCHANT.into(),
            api_key: "sk_test_statement_fixture".into(),
            webhook_secret: "whsec_local_statement_fixture_only".into(),
        },
        addr,
    )
    .expect("sandbox transport");
    let api = Payments::new(identity, transport);
    let remote = Router::new()
        .route("/v1/account", get(account))
        .route("/v1/balance_transactions", get(page))
        .with_state(r.clone());
    let (end, receive) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(socket, remote)
            .with_graceful_shutdown(async {
                let _ = receive.await;
            })
            .await
            .expect("fixture stopped")
    });
    let delayed = {
        let api = api.clone();
        tokio::spawn(async move { api.scan_statement_page().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(6), r.arrived.notified())
        .await
        .expect("old claimant at merchant");
    assert!(!api.scan_statement_page().await.expect("one claim"));
    sqlx::query("UPDATE test_statement_sync SET claim_until=clock_timestamp()-interval '1 second' WHERE merchant_id=$1").bind(MERCHANT).execute(db.pool()).await.expect("fixture claim expiration");
    r.phase.store(0, Ordering::SeqCst);
    assert!(api.scan_statement_page().await.expect("replacement page"));
    r.resume.notify_one();
    delayed
        .await
        .expect("join")
        .expect("discard stale response");
    r.phase.store(2, Ordering::SeqCst);
    assert!(
        api.scan_statement_page().await.is_err(),
        "provider failure cannot seal the scan"
    );
    let checkpoint: (Option<i64>, String, i32) = sqlx::query_as(
        "SELECT high_water,page_cursor,failures FROM test_statement_sync WHERE merchant_id=$1",
    )
    .bind(MERCHANT)
    .fetch_one(db.pool())
    .await
    .expect("cursor");
    assert_eq!(checkpoint, (None, "txn_last_a".into(), 1));
    let journal: (String, String, String) =
        sqlx::query_as("SELECT phase,page_cursor,reason FROM test_statement_failures")
            .fetch_one(db.pool())
            .await
            .expect("durable failed attempt");
    assert_eq!(
        journal,
        (
            "external".into(),
            "txn_last_a".into(),
            "scan_unavailable".into()
        )
    );
    r.phase.store(0, Ordering::SeqCst);
    sqlx::query("UPDATE test_statement_sync SET next_check=clock_timestamp() WHERE merchant_id=$1")
        .bind(MERCHANT)
        .execute(db.pool())
        .await
        .expect("accelerate test retry");
    assert!(api.scan_statement_page().await.expect("original cursor"));
    assert!(
        api.scan_statement_page()
            .await
            .expect("check obligations, seal")
    );
    assert!(
        api.scan_statement_page()
            .await
            .expect("review old holds and seal")
    );
    assert!(
        api.scan_statement_page()
            .await
            .expect("check holds after job audit")
    );
    let first: Value =
        sqlx::query_scalar("SELECT report FROM test_statement_runs WHERE until_unix=$1")
            .bind(start + 86400)
            .fetch_one(db.pool())
            .await
            .expect("first sealed report");
    assert_eq!(first["observed_scan_issues"], "6");
    let issue_types: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM test_statement_issues ORDER BY kind")
            .fetch_all(db.pool())
            .await
            .expect("diagnosis");
    assert_eq!(
        issue_types,
        [
            "amount_mismatch",
            "cross_currency_review",
            "external_refund_without_posting",
            "missing_external",
            "multiple_external",
            "unattributed_external"
        ]
    );
    let cases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM reconciliation_cases WHERE owner_id=$1")
            .bind(&owner)
            .fetch_one(db.pool())
            .await
            .expect("owned cases");
    assert_eq!(cases, 6);
    for _ in 0..5 {
        assert!(api.scan_statement_page().await.expect("overlap scan"));
    }
    assert!(!api.scan_statement_page().await.expect("caught up"));
    let last = sqlx::query("SELECT run_id,report FROM test_statement_runs WHERE until_unix=$1")
        .bind(start + 172800)
        .fetch_one(db.pool())
        .await
        .expect("second report");
    let report: Value = last.get("report");
    assert_eq!(report["from_unix"], (start + 86400).to_string());
    assert_eq!(
        report["merchant_statement_by_settlement_currency"],
        json!([{"currency":"hkd","category":"charge","provider_status":"available","lines":"1","gross_minor":"350","fee_minor":"11","net_minor":"339"}])
    );
    let good: i64 =
        sqlx::query_scalar("SELECT fee FROM test_statement_lines WHERE line_id='txn_last_a'")
            .fetch_one(db.pool())
            .await
            .expect("original preserved");
    assert_eq!(good, 12);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reconciliation_cases WHERE owner_id=$1")
            .bind(&owner)
            .fetch_one(db.pool())
            .await
            .expect("no duplicate cases"),
        cases + 1
    );
    assert_eq!(
        proof(&db, &owner).await,
        before,
        "scanner cannot post funds"
    );
    let ac: OidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("Admin config"))
            .expect("config"),
    )
    .expect("Admin OIDC");
    let platform = operator(&db, &ac.issuer, "platform", "platform").await;
    let tenant = operator(&db, &ac.issuer, "tenant", &owner).await;
    let origin = ac.admin_origin.clone();
    let issuer = ac.issuer.clone();
    let admin = routes::router(
        AdminIdentity::discover(db.pool().clone(), ac)
            .await
            .expect("real Admin discovery"),
    );
    let scope = "?environment=dev&scope_kind=platform&scope_id=platform";
    assert_eq!(
        read(
            &admin,
            &tenant,
            &format!("/admin/v1/test-statements{scope}")
        )
        .await
        .0,
        403
    );
    let (status, history) = read(
        &admin,
        &platform,
        &format!("/admin/v1/test-statements{scope}&limit=1"),
    )
    .await;
    assert_eq!(status, 200, "{history}");
    assert_eq!(history["data"][0]["sealed"], true);
    let id: String = last.get("run_id");
    let (status, detail) = read(
        &admin,
        &platform,
        &format!("/admin/v1/test-statements/{id}{scope}&limit=1"),
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["report"], report);
    assert!(detail["next_cursor"].is_string());
    let key: String = sqlx::query_scalar(
        "SELECT issue_key FROM test_statement_issues WHERE kind='amount_mismatch'",
    )
    .fetch_one(db.pool())
    .await
    .expect("currency issue");
    let path = format!(
        "/admin/v1/test-statement-issues/{key}?run_id={id}&environment=dev&scope_kind=tenant&scope_id={owner}"
    );
    let (status, v) = read(&admin, &tenant, &path).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["data"][0]["detail"]["observed_minor"], "775");
    let outsider = operator(&db, &issuer, "tenant", "different-owner").await;
    assert_eq!(
        read(&admin, &outsider, &path.replace(&owner, "different-owner"))
            .await
            .0,
        404
    );
    let unknown: String = sqlx::query_scalar(
        "SELECT issue_key FROM test_statement_issues WHERE business_ref='txn_foreign'",
    )
    .fetch_one(db.pool())
    .await
    .expect("unattributed line");
    assert_eq!(
        read(&admin, &tenant, &path.replace(&key, &unknown)).await.0,
        404
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM test_refund_orders WHERE refund_id=$1")
            .bind(&rid)
            .fetch_one(db.pool())
            .await
            .expect("no silent refund repair"),
        "pending"
    );
    let case = v["case_id"].as_str().expect("owned mismatch case");
    if let Ok(driver) = std::env::var("ORANGE_STATEMENT_BROWSER") {
        let dir = std::path::PathBuf::from(
            std::env::var("ORANGE_STATEMENT_EVIDENCE").expect("screenshots"),
        );
        browser(admin.clone(),&driver,json!({"origin":origin,"platform":platform,"tenant":tenant,"run":id,"issue":key,"case":case,
            "report_screenshot":dir.join("daily-report.png"),"tenant_screenshot":dir.join("tenant-evidence.png")})).await;
    }
    sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=(SELECT principal_id FROM admin_sessions WHERE token_hash=$1)").bind(hash_secret(&platform)).execute(db.pool()).await.expect("revoke platform reader");
    assert_eq!(
        read(
            &admin,
            &platform,
            &format!("/admin/v1/test-statements/{id}{scope}")
        )
        .await
        .0,
        403
    );
    let _ = end.send(());
    server.await.expect("merchant joined");
}

async fn browser(app: Router, driver: &str, mut fixture: Value) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Admin API");
    fixture["api"] = json!(format!(
        "http://{}",
        listener.local_addr().expect("Admin port")
    ));
    let (stop, done) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .expect("Admin served");
    });
    let mut child =
        tokio::process::Command::new(std::env::var("ORANGE_STATEMENT_NODE").expect("pinned Node"))
            .arg(driver)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("browser process");
    let mut pipe = child.stdin.take().expect("fixture pipe");
    pipe.write_all(&serde_json::to_vec(&fixture).expect("fixture"))
        .await
        .expect("private fixture delivery");
    drop(pipe);
    let status = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait())
        .await
        .expect("browser deadline")
        .expect("browser exit");
    let _ = stop.send(());
    task.await.expect("Admin joined");
    assert!(status.success(), "real statement browser");
}
