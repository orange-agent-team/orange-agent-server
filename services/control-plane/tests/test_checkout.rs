//! Actual PostgreSQL, current user HTTP and Stripe wire adapter against an isolated loopback merchant double.
#[path = "support/refund_admin.rs"]
mod admin_fixture;
#[path = "support/finance_sessions.rs"]
mod finance_sessions;
#[path = "support/stripe_merchant.rs"]
mod merchant;
#[path = "support/purchase_lot.rs"]
mod purchase_lot;
#[path = "test_checkout/refunds.rs"]
mod refunds;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use hmac::{Hmac, KeyInit, Mac};
use merchant::{MERCHANT, Provider, SIGNING, config};
use orange_control_plane::{
    auth::users::{UserIdentity, UserOidcConfig},
    payments::{Payments, Sandbox},
    store::Store,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::Ordering};
use tower::ServiceExt;
use uuid::Uuid;
struct Buyer {
    user: String,
    owner: String,
    token: String,
    csrf: String,
}
async fn buyer(db: &Store) -> Buyer {
    let b = Buyer {
        user: Uuid::new_v4().to_string(),
        owner: Uuid::new_v4().to_string(),
        token: Uuid::new_v4().simple().to_string(),
        csrf: Uuid::new_v4().simple().to_string(),
    };
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&b.owner)
        .execute(db.pool())
        .await
        .expect("fixture owner");
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
        .bind(&b.owner)
        .execute(db.pool())
        .await
        .expect("fixture wallet");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'checkout-test',$1,$2)").bind(&b.user).bind(&b.owner).execute(db.pool()).await.expect("fixture buyer");
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,0,floor(extract(epoch FROM clock_timestamp()))::bigint+600,floor(extract(epoch FROM clock_timestamp()))::bigint+900)")
        .bind(hex::encode(Sha256::digest(b.token.as_bytes()))).bind(Uuid::new_v4().to_string()).bind(&b.user).bind(&b.csrf).execute(db.pool()).await.expect("fixture session");
    b
}
async fn user(
    (app, origin): (&Router, &str),
    b: &Buyer,
    method: &str,
    url: &str,
    key: &str,
    body: Value,
    status: u16,
) -> Value {
    let r = Request::builder()
        .uri(url)
        .method(method)
        .header("cookie", format!("__Host-orange-user={}", b.token))
        .header("origin", origin)
        .header("x-csrf-token", &b.csrf)
        .header("idempotency-key", key)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build user request");
    let resp = app.clone().oneshot(r).await.expect("user router");
    let code = resp.status().as_u16();
    let v: Value = serde_json::from_slice(
        &to_bytes(resp.into_body(), 65536)
            .await
            .expect("bounded response"),
    )
    .expect("JSON response");
    assert_eq!(code, status, "{url}: {v}");
    v
}
async fn webhook(app: &Router, event: &Value, key: &str, t: i64, alter: bool) -> u16 {
    let bytes = event.to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("fixture HMAC");
    mac.update(format!("{t}.{bytes}").as_bytes());
    let tag = hex::encode(mac.finalize().into_bytes());
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/payment-events/stripe-test")
                .header("stripe-signature", format!("t={t},v1={tag}"))
                .body(Body::from(if alter { bytes + " " } else { bytes }))
                .expect("signed event request"),
        )
        .await
        .expect("webhook router")
        .status()
        .as_u16()
}
async fn wallet(db: &Store, owner: &str) -> (i64, i64, i64) {
    sqlx::query_as("SELECT gift,earned,purchased FROM wallets WHERE owner_id=$1")
        .bind(owner)
        .fetch_one(db.pool())
        .await
        .expect("wallet buckets")
}

#[tokio::test]
async fn checkout_unknown_concurrency_raw_signatures_and_authoritative_test_credit_fulfillment() {
    let db =
        Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("separate PostgreSQL"))
            .await
            .expect("PostgreSQL");
    db.migrate().await.expect("migrations");
    let cfg: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(
            std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity configuration path"),
        )
        .expect("identity configuration"),
    )
    .expect("OIDC settings");
    let origin = cfg.user_origin.clone();
    let identity = UserIdentity::discover(db.pool().clone(), cfg)
        .await
        .expect("development issuer");
    let b = buyer(&db).await;
    let other = buyer(&db).await;
    let (p, addr, stop, server) = merchant::start().await;
    let mut live = config();
    live.api_key = "sk_live_wrong_mode".into();
    assert!(Sandbox::loopback_fixture(live, addr).is_err());
    let api = Payments::new(
        identity.clone(),
        Sandbox::loopback_fixture(config(), addr).expect("sandbox transport"),
    );
    let app = api
        .router()
        .merge(orange_control_plane::auth::routes::router(identity));
    sqlx::query(
        "INSERT INTO test_credit_offers VALUES($1,'dev-pack','price_devUnits','usd',500,9000,true)",
    )
    .bind(MERCHANT)
    .execute(db.pool())
    .await
    .expect("admitted offer");
    let base = format!("/api/v1/owners/{}", b.owner);
    let orders = format!("{base}/checkout-orders");
    let rates = user(
        (&app, &origin),
        &b,
        "GET",
        &format!("{base}/credit-offers"),
        "",
        Value::Null,
        200,
    )
    .await;
    assert_eq!(rates[0]["microcredits"], "9000");
    let cancelled = Uuid::new_v4().to_string();
    user(
        (&app, &origin),
        &b,
        "POST",
        &orders,
        &cancelled,
        json!({"order_id":cancelled,"offer_id":"dev-pack"}),
        201,
    )
    .await;
    p.mode.store(7, Ordering::SeqCst);
    user(
        (&app, &origin),
        &b,
        "POST",
        &format!("{orders}/{cancelled}/session"),
        &cancelled,
        json!({}),
        409,
    )
    .await;
    assert_eq!(
        user(
            (&app, &origin),
            &b,
            "GET",
            &format!("{orders}/{cancelled}"),
            "",
            Value::Null,
            200
        )
        .await["state"],
        "created"
    );
    p.mode.store(0, Ordering::SeqCst);
    let cancel = format!("{orders}/{cancelled}/cancel");
    for _ in 0..2 {
        assert_eq!(
            user(
                (&app, &origin),
                &b,
                "POST",
                &cancel,
                &cancelled,
                json!({}),
                200
            )
            .await["state"],
            "cancelled"
        );
    }
    assert_eq!(
        user(
            (&app, &origin),
            &b,
            "POST",
            &format!("{orders}/{cancelled}/session"),
            &cancelled,
            json!({}),
            200
        )
        .await["state"],
        "cancelled"
    );
    assert_eq!(p.posts.load(Ordering::SeqCst), 0);
    let mut first_paid = None;
    for (mode, expected) in [
        (0, "open"),
        (1, "unknown"),
        (2, "open"),
        (3, "open"),
        (4, "open"),
        (5, "open"),
        (6, "open"),
    ] {
        p.mode.store(mode, Ordering::SeqCst);
        let id = Uuid::new_v4().to_string();
        if mode == 0 {
            first_paid = Some(id.clone());
        }
        let path = format!("{orders}/{id}");
        let body = json!({"order_id":id,"offer_id":"dev-pack"});
        let mut bad = body.clone();
        bad["amount_minor"] = json!(1);
        user((&app, &origin), &b, "POST", &orders, &id, bad, 422).await;
        user(
            (&app, &origin),
            &other,
            "POST",
            &orders,
            &id,
            body.clone(),
            403,
        )
        .await;
        user(
            (&app, "https://foreign.invalid"),
            &b,
            "POST",
            &orders,
            &id,
            body.clone(),
            403,
        )
        .await;
        let first = user((&app, &origin), &b, "POST", &orders, &id, body.clone(), 201).await;
        assert_eq!(first["state"], "created");
        assert_eq!(
            first,
            user((&app, &origin), &b, "POST", &orders, &id, body, 201).await
        );
        let start = format!("{path}/session");
        let prior = p.posts.load(Ordering::SeqCst);
        let (a, c) = tokio::join!(
            user((&app, &origin), &b, "POST", &start, &id, json!({}), 200),
            user((&app, &origin), &b, "POST", &start, &id, json!({}), 200)
        );
        assert!([&a, &c].iter().any(|x| x["state"] == expected));
        assert_eq!(p.posts.load(Ordering::SeqCst), prior + 1);
        let current = user((&app, &origin), &b, "POST", &start, &id, json!({}), 200).await;
        assert_eq!(current["state"], expected);
        assert_eq!(p.posts.load(Ordering::SeqCst), prior + 1);
        user((&app, &origin), &other, "GET", &path, "", Value::Null, 403).await;
        let sid = format!("cs_test_{}", id.replace('-', ""));
        let event = json!({"id":format!("evt_{}",id.replace('-',"")),"object":"event","livemode":false,"type":"checkout.session.completed","data":{"object":{"id":sid,"object":"checkout.session","metadata":{"orange_order_id":id},"payment_status":"paid","amount_total":999999}}});
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(webhook(&app, &event, "wrong", ts, false).await, 403);
        assert_eq!(webhook(&app, &event, SIGNING, ts, true).await, 403);
        assert_eq!(webhook(&app, &event, SIGNING, ts - 601, false).await, 403);
        let mut foreign = event.clone();
        foreign["account"] = json!("acct_other");
        assert_eq!(webhook(&app, &foreign, SIGNING, ts, false).await, 403);
        let initial = wallet(&db, &b.owner).await;
        let (w1, w2) = tokio::join!(
            webhook(&app, &event, SIGNING, ts, false),
            webhook(&app, &event, SIGNING, ts, false)
        );
        assert_eq!((w1, w2), (204, 204));
        assert_eq!(
            wallet(&db, &b.owner).await,
            initial,
            "signed event alone cannot mint"
        );
        let (x, y) = tokio::join!(api.process_one(), api.process_one());
        assert_eq!(
            u8::from(x.expect("first reconciliation"))
                + u8::from(y.expect("second reconciliation")),
            1
        );
        let state = user((&app, &origin), &b, "GET", &path, "", Value::Null, 200).await;
        if mode >= 2 {
            assert_eq!(state["state"], "review_required");
            assert_eq!(wallet(&db, &b.owner).await, initial);
        } else {
            assert_eq!(state["state"], "credited");
            assert!(state["checkout_url"].is_null());
            assert_eq!(
                wallet(&db, &b.owner).await,
                (initial.0, initial.1, initial.2 + 9000)
            );
            let ledger:(i64,i64)=sqlx::query_as("SELECT count(DISTINCT transaction_id),sum(amount)::bigint FROM ledger_transactions JOIN ledger_entries USING(transaction_id) WHERE business_key=$1").bind(format!("test-checkout:{id}")).fetch_one(db.pool()).await.expect("balanced ledger");
            assert_eq!(ledger, (1, 0));
        }
        let mut late = event.clone();
        late["id"] = json!(format!("evt_late{}", id.replace('-', "")));
        late["type"] = json!("checkout.session.expired");
        let before = wallet(&db, &b.owner).await;
        assert_eq!(webhook(&app, &late, SIGNING, ts, false).await, 204);
        assert!(api.process_one().await.expect("late event recovery"));
        assert_eq!(wallet(&db, &b.owner).await, before);
    }
    p.mode.store(0, Ordering::SeqCst);
    let early = Uuid::new_v4().to_string();
    let suffix = early.replace('-', "");
    let path = format!("{orders}/{early}");
    user(
        (&app, &origin),
        &b,
        "POST",
        &orders,
        &early,
        json!({"order_id":early,"offer_id":"dev-pack"}),
        201,
    )
    .await;
    user(
        (&app, &origin),
        &b,
        "POST",
        &format!("{path}/session"),
        &early,
        json!({}),
        200,
    )
    .await;
    *p.disputed_order.lock().expect("fixture original dispute") = Some(early.clone());
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let before = wallet(&db, &b.owner).await;
    let disputed = json!({"id":"evt_before_credit","object":"event","livemode":false,"type":"charge.dispute.created","data":{"object":{"object":"dispute","id":"dp_before_credit","charge":format!("ch_{suffix}")}}});
    assert_eq!(webhook(&app, &disputed, SIGNING, ts, false).await, 204);
    let completion = json!({"id":"evt_complete_after_dispute","object":"event","livemode":false,"type":"checkout.session.completed","data":{"object":{"id":format!("cs_test_{suffix}"),"object":"checkout.session","metadata":{"orange_order_id":early}}}});
    let mut reused = completion.clone();
    reused["id"] = json!("evt_before_credit");
    assert_eq!(
        webhook(&app, &reused, SIGNING, ts, false).await,
        409,
        "one event identity cannot be reused across different handlers"
    );
    assert_eq!(webhook(&app, &completion, SIGNING, ts, false).await, 204);
    api.process_one()
        .await
        .expect("source-matched payment cannot mint before dispute check");
    assert_eq!(
        user((&app, &origin), &b, "GET", &path, "", Value::Null, 200).await["state"],
        "review_required"
    );
    assert_eq!(wallet(&db, &b.owner).await, before);
    api.process_dispute()
        .await
        .expect("actual provider says won");
    assert_eq!(
        user(
            (&app, &origin),
            &b,
            "POST",
            &format!("{path}/reconcile"),
            &early,
            json!({}),
            200
        )
        .await["state"],
        "credited"
    );
    assert_eq!(
        wallet(&db, &b.owner).await,
        (before.0, before.1, before.2 + 9000)
    );
    let organization = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&organization)
        .execute(db.pool())
        .await
        .expect("team owner");
    sqlx::query(
        "INSERT INTO organizations(organization_id,name) VALUES($1,'Checkout finance fixture')",
    )
    .bind(&organization)
    .execute(db.pool())
    .await
    .expect("organization");
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
        .bind(&organization)
        .execute(db.pool())
        .await
        .expect("team wallet");
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'billing'),($1,$3,'admin')").bind(&organization).bind(&b.user).bind(&other.user).execute(db.pool()).await.expect("team roles");
    let org_path = format!("/api/v1/owners/{organization}/checkout-orders");
    let id = Uuid::new_v4().to_string();
    let request = json!({"order_id":id,"offer_id":"dev-pack"});
    user(
        (&app, &origin),
        &other,
        "POST",
        &org_path,
        &id,
        request.clone(),
        403,
    )
    .await;
    assert_eq!(
        user((&app, &origin), &b, "POST", &org_path, &id, request, 201).await["owner_id"],
        organization
    );
    sqlx::query("UPDATE memberships SET active=false WHERE organization_id=$1 AND user_id=$2")
        .bind(&organization)
        .bind(&b.user)
        .execute(db.pool())
        .await
        .expect("finance grant withdrawn");
    user(
        (&app, &origin),
        &b,
        "POST",
        &format!("{org_path}/{id}/session"),
        &id,
        json!({}),
        403,
    )
    .await;
    assert_eq!(wallet(&db, &organization).await, (0, 0, 0));
    if let Ok(driver) = std::env::var("ORANGE_CHECKOUT_BROWSER") {
        p.mode.store(0, Ordering::SeqCst);
        let before = wallet(&db, &b.owner).await;
        let requests = p.posts.load(Ordering::SeqCst);
        browser_flow(app.clone(),&driver,json!({"origin":origin,"token":b.token,"owner":b.owner,"refunds":true,"screenshot":std::env::var("ORANGE_CHECKOUT_SCREENSHOT").expect("screenshot")})).await;
        assert_eq!(p.posts.load(Ordering::SeqCst), requests + 1);
        assert_eq!(
            wallet(&db, &b.owner).await,
            (before.0, before.1, before.2 + 9000)
        );
    }
    refunds::exercise(
        &db,
        &api,
        &app,
        &b,
        &origin,
        &p,
        first_paid.as_deref().expect("first paid purchase"),
    )
    .await;
    let original = first_paid.as_deref().expect("first paid purchase");
    let charge = format!("ch_{}", original.replace('-', ""));
    *p.disputed_order.lock().expect("merchant dispute state") = Some(original.into());
    let event = json!({"id":"evt_web_spend_pause","object":"event","livemode":false,"type":"charge.dispute.created","data":{"object":{"id":"dp_web_pause","object":"dispute","charge":charge}}});
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    assert_eq!(webhook(&app, &event, SIGNING, ts, false).await, 204);
    let status_path = format!("{base}/spending-status");
    assert_eq!(
        user(
            (&app, &origin),
            &b,
            "GET",
            &status_path,
            "",
            Value::Null,
            200
        )
        .await["positive_spending"],
        "paused"
    );
    user(
        (&app, &origin),
        &other,
        "GET",
        &status_path,
        "",
        Value::Null,
        403,
    )
    .await;
    let blocked = Uuid::new_v4().to_string();
    user(
        (&app, &origin),
        &b,
        "POST",
        &orders,
        &blocked,
        json!({"order_id":blocked,"offer_id":"dev-pack"}),
        201,
    )
    .await;
    let charged_before = p.posts.load(Ordering::SeqCst);
    let denied = user(
        (&app, &origin),
        &b,
        "POST",
        &format!("{orders}/{blocked}/session"),
        &blocked,
        json!({}),
        409,
    )
    .await;
    assert_eq!(denied["code"], "SPENDING_PAUSED");
    assert_eq!(p.posts.load(Ordering::SeqCst), charged_before);
    if let Ok(driver) = std::env::var("ORANGE_CHECKOUT_BROWSER") {
        let screenshot = std::path::PathBuf::from(
            std::env::var("ORANGE_CHECKOUT_SCREENSHOT").expect("evidence"),
        )
        .with_file_name("spending-pause.png");
        browser_flow(app.clone(),&driver,json!({"origin":origin,"token":b.token,"owner":b.owner,"order":original,"spending_paused":true,"screenshot":screenshot})).await;
    }
    assert!(
        api.process_dispute()
            .await
            .expect("resume using original merchant state")
    );
    assert_eq!(
        user(
            (&app, &origin),
            &b,
            "GET",
            &status_path,
            "",
            Value::Null,
            200
        )
        .await["positive_spending"],
        "available"
    );
    stop.send(()).expect("signal shutdown");
    server.await.expect("join fixture");
}

async fn browser_flow(app: Router, driver: &str, mut fixture: Value) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local API");
    fixture["api"] = json!(format!(
        "http://{}",
        listener.local_addr().expect("API port")
    ));
    let (stop, ended) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = ended.await;
            })
            .await
            .expect("user API")
    });
    let mut child = tokio::process::Command::new(
        std::env::var(if fixture.get("maker").is_some() {
            "ORANGE_REFUND_ADMIN_NODE"
        } else {
            "ORANGE_CHECKOUT_NODE"
        })
        .expect("pinned Node"),
    )
    .arg(driver)
    .stdin(std::process::Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .expect("browser driver");
    let mut input = child.stdin.take().expect("fixture pipe");
    input
        .write_all(&serde_json::to_vec(&fixture).expect("fixture"))
        .await
        .expect("private fixture delivery");
    drop(input);
    let status = tokio::time::timeout(std::time::Duration::from_secs(75), child.wait())
        .await
        .expect("browser deadline")
        .expect("browser exit");
    let _ = stop.send(());
    service.await.expect("local server stopped");
    assert!(status.success(), "real checkout UI");
}

async fn job_from_wallet(db: &Store, owner: &str, root: &str, held: i64) -> String {
    use orange_protocol::{InvocationPurpose, SubmitJob};
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let projection = "c".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'lot-reservation-fixture',$6,clock_timestamp()+interval '5 minutes')")
        .bind(&quote).bind(owner).bind(root).bind(&invocation).bind(&projection).bind(held).execute(db.pool()).await.expect("approved queue fixture");
    db.submit(
        owner,
        &Uuid::new_v4().to_string(),
        &SubmitJob {
            quote_id: quote,
            root_budget_id: root.into(),
            purpose: InvocationPurpose::Primary,
            invocation_id: invocation,
            request_projection_digest: projection,
        },
    )
    .await
    .expect("normal job reservation")
    .job_id
}
async fn lot(db: &Store, order: &str) -> (i64, i64, i64) {
    sqlx::query_as("SELECT available,job_held,spent FROM test_purchase_lots WHERE order_id=$1")
        .bind(order)
        .fetch_one(db.pool())
        .await
        .expect("purchase lot")
}

#[tokio::test]
async fn schema_58_purchase_and_job_history_survives_lot_upgrade_and_exact_queue_release() {
    let url = std::env::var("ORANGE_CHECKOUT_OLD_DATABASE").expect("disposable historic database");
    assert!(
        url.rsplit('/')
            .next()
            .is_some_and(|v| v.starts_with("orange_checkout_old_"))
    );
    // Exercise the prepared statements on the same PostgreSQL connection across the upgrade.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("historical connection");
    let db = Store::from_pool(pool);
    sqlx::migrate::Migrator::with_migrations(
        sqlx::migrate!()
            .iter()
            .filter(|m| m.version <= 58)
            .cloned()
            .collect(),
    )
    .run(db.pool())
    .await
    .expect("schema 58");
    let b = buyer(&db).await;
    sqlx::query("UPDATE wallets SET purchased=100 WHERE owner_id=$1")
        .bind(&b.owner)
        .execute(db.pool())
        .await
        .expect("pre-existing unattributed balance");
    let a = purchase_lot::credited(db.pool(), &b.owner, &b.user, MERCHANT, "first", 1000).await;
    let c = purchase_lot::credited(db.pool(), &b.owner, &b.user, MERCHANT, "second", 2000).await;
    let root = db
        .create_budget(
            &b.owner,
            "lot-root",
            &orange_protocol::CreateBudget {
                scope_id: "lots".into(),
                limit: "10000".to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("budget")
        .root_budget_id;
    let historic = job_from_wallet(&db, &b.owner, &root, 1600).await;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(db.pool())
        .await
        .expect("legacy backend");
    let reader = orange_control_plane::events::EventStore::new(db.pool().clone());
    let stream = reader
        .discover(&b.owner, "job", &historic)
        .await
        .expect("legacy stream");
    let first = reader
        .read(&b.owner, &stream.stream_id, "0", 4)
        .await
        .expect("legacy events");
    assert_eq!(first.events.len(), 1);
    db.migrate().await.expect("actual migration");
    db.migrate().await.expect("repeat migrations");
    let after: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(db.pool())
        .await
        .expect("upgraded backend");
    assert_eq!(pid, after);
    let upgraded = reader
        .read(&b.owner, &stream.stream_id, "0", 4)
        .await
        .expect("warm event projection");
    assert_eq!(first.events[0].event_id, upgraded.events[0].event_id);
    assert_eq!(
        first.events[0].payload_digest,
        upgraded.events[0].payload_digest
    );
    assert_eq!(lot(&db, &a).await, (0, 1000, 0));
    assert_eq!(lot(&db, &c).await, (1500, 500, 0));
    for _ in 0..2 {
        db.cancel_queued(&b.owner, &historic, "cancel-old")
            .await
            .expect("cancel original job");
    }
    assert_eq!(lot(&db, &a).await, (1000, 0, 0));
    assert_eq!(lot(&db, &c).await, (2000, 0, 0));
    let later = job_from_wallet(&db, &b.owner, &root, 1300).await;
    assert_eq!(lot(&db, &a).await, (0, 1000, 0));
    assert_eq!(lot(&db, &c).await, (1800, 200, 0));
    db.cancel_queued(&b.owner, &later, "cancel-later")
        .await
        .expect("return original lots");
    assert_eq!(wallet(&db, &b.owner).await, (0, 0, 3100));
    let total:(i64,i64)=sqlx::query_as("SELECT sum(a.held)::bigint,sum(a.charged)::bigint FROM test_job_lot_allocations a WHERE job_id IN ($1,$2)").bind(historic).bind(later).fetch_one(db.pool()).await.expect("immutable allocations");
    assert_eq!(total, (2700, 0));
}
