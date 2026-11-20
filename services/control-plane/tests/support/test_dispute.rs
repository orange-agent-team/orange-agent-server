//! Real chargeback restriction, historical spent lot and fenced authoritative reads; test merchant is loopback only.
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Request},
    routing::get,
};
use hmac::{Hmac, KeyInit, Mac};
use orange_control_plane::{
    auth::users::UserIdentity,
    payments::{Payments, Sandbox, SandboxConfig},
    store::{Store, StoreError},
};
use orange_protocol::{CreateBudget, InvocationPurpose, SubmitJob};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;
struct Merchant {
    id: String,
    charge: String,
    intent: String,
    state: Mutex<String>,
    pause: AtomicBool,
    arrived: Notify,
    resume: Notify,
}
async fn account(State(m): State<Arc<Merchant>>, h: HeaderMap) -> Json<Value> {
    assert!(h.contains_key("authorization"));
    Json(json!({"id":m.id,"object":"account"}))
}
async fn dispute(
    State(m): State<Arc<Merchant>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> Json<Value> {
    assert!(h.contains_key("authorization"));
    let state = m.state.lock().expect("external state").clone();
    if m.pause.swap(false, Ordering::SeqCst) {
        m.arrived.notify_one();
        m.resume.notified().await;
    }
    Json(
        json!({"id":id,"object":"dispute","livemode":false,"charge":m.charge,"payment_intent":m.intent,"amount":500,"currency":"usd","status":state}),
    )
}
const SIGNING: &str = "whsec_local_dispute_fixture_only";
async fn notify(app: &Router, id: &str, dp: &str, charge: &str, status: &str, tamper: bool) -> u16 {
    let bytes=json!({"id":id,"object":"event","livemode":false,"type":"charge.dispute.updated","data":{"object":{"object":"dispute","id":dp,"charge":charge,"status":status}}}).to_string();
    let t = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut mac = Hmac::<Sha256>::new_from_slice(SIGNING.as_bytes()).expect("test signature");
    mac.update(format!("{t}.{bytes}").as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    app.clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/payment-events/stripe-test")
                .method("POST")
                .header("stripe-signature", format!("t={t},v1={signature}"))
                .body(Body::from(if tamper { bytes + " " } else { bytes }))
                .expect("notification"),
        )
        .await
        .expect("router")
        .status()
        .as_u16()
}
async fn quote(db: &Store, owner: &str, root: &str, amount: i64) -> SubmitJob {
    let quote_id = Uuid::new_v4().to_string();
    let invocation_id = Uuid::new_v4().to_string();
    let projection = "a".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'chargeback-test',$6,clock_timestamp()+interval '5 minutes')")
        .bind(&quote_id).bind(owner).bind(root).bind(&invocation_id).bind(&projection).bind(amount).execute(db.pool()).await.expect("isolated quote");
    SubmitJob {
        quote_id,
        invocation_id,
        root_budget_id: root.into(),
        request_projection_digest: projection,
        purpose: InvocationPurpose::Primary,
    }
}
pub async fn after_actual_spend(
    db: &Store,
    identity: &UserIdentity,
    owner: &str,
    order: &str,
    job: &str,
) {
    let (charge,intent,merchant):(String,String,String)=sqlx::query_as("SELECT f.charge_id,f.payment_intent,o.merchant_id FROM test_credit_fulfillments f JOIN test_checkout_orders o USING(order_id) WHERE order_id=$1 AND f.owner_id=$2")
        .bind(order).bind(owner).fetch_one(db.pool()).await.expect("original cash fact");
    let m = Arc::new(Merchant {
        id: merchant.clone(),
        charge: charge.clone(),
        intent,
        state: Mutex::new("under_review".into()),
        pause: AtomicBool::new(false),
        arrived: Notify::new(),
        resume: Notify::new(),
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock merchant");
    let addr = socket.local_addr().expect("address");
    let api = Payments::new(
        identity.clone(),
        Sandbox::loopback_fixture(
            SandboxConfig {
                merchant_id: merchant,
                api_key: "sk_test_dispute_fixture".into(),
                webhook_secret: SIGNING.into(),
            },
            addr,
        )
        .expect("only loopback test credentials"),
    );
    let router = api.router();
    let remote = Router::new()
        .route("/v1/account", get(account))
        .route("/v1/disputes/{id}", get(dispute))
        .with_state(m.clone());
    let (stop, end) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(socket, remote)
            .with_graceful_shutdown(async {
                let _ = end.await;
            })
            .await
            .expect("merchant stopped")
    });
    let snapshots = || {
        sqlx::query_as::<_,(i64,i64,i64,i64,String)>("SELECT w.purchased,w.reserved_purchased,l.spent,s.charge,s.transaction_id FROM wallets w JOIN test_purchase_lots l USING(owner_id) CROSS JOIN job_settlements s WHERE l.order_id=$1 AND s.job_id=$2")
        .bind(order).bind(job).fetch_one(db.pool())
    };
    let original = snapshots().await.expect("actual prior settlement");
    assert_eq!(
        (original.0, original.1, original.2, original.3),
        (7800, 0, 2200, 2200)
    );
    let root = db
        .create_budget(
            owner,
            "dispute-root",
            &CreateBudget {
                scope_id: "dispute-scope".into(),
                limit: "100".to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("budget")
        .root_budget_id;
    let priced = quote(db, owner, &root, 10).await;
    assert_eq!(
        notify(&router, "evt_one", "dp_one", &charge, "won", true).await,
        403
    );
    for _ in 0..2 {
        assert_eq!(
            notify(&router, "evt_one", "dp_one", &charge, "won", false).await,
            204
        );
    }
    assert!(matches!(
        db.submit(owner, "paid-after-notice", &priced).await,
        Err(StoreError::SpendingPaused)
    ));
    assert_eq!(snapshots().await.expect("rolled back reserve"), original);
    // A stale in-flight retrieval with a released claim cannot clear the restriction.
    *m.state.lock().expect("external state") = "won".into();
    m.pause.store(true, Ordering::SeqCst);
    let delayed = {
        let api = api.clone();
        tokio::spawn(async move { api.process_dispute().await })
    };
    tokio::time::timeout(Duration::from_secs(5), m.arrived.notified())
        .await
        .expect("captured earlier provider state");
    *m.state.lock().expect("external state") = "lost".into();
    assert_eq!(
        notify(&router, "evt_two", "dp_one", &charge, "won", false).await,
        204
    );
    sqlx::query("UPDATE test_dispute_work SET claim_until=clock_timestamp()-interval '1 second',next_check=clock_timestamp() WHERE dispute_id='dp_one'").execute(db.pool()).await.expect("expire only test claim");
    assert!(
        api.process_dispute()
            .await
            .expect("current authoritative loss")
    );
    m.resume.notify_one();
    delayed
        .await
        .expect("joined")
        .expect("stale reader discarded");
    let status:(String,bool,i64)=sqlx::query_as("SELECT state,blocks_spend,(SELECT count(*) FROM test_dispute_observations WHERE dispute_id='dp_one') FROM test_dispute_work WHERE dispute_id='dp_one'").fetch_one(db.pool()).await.expect("durable loss");
    assert_eq!(status, ("lost".into(), true, 1));
    assert_eq!(
        snapshots().await.expect("preserved historical spend"),
        original
    );
    assert_eq!(
        notify(&router, "evt_three", "dp_two", &charge, "won", false).await,
        204
    );
    *m.state.lock().expect("external state") = "won".into();
    assert!(api.process_dispute().await.expect("second dispute won"));
    assert!(
        matches!(
            db.submit(owner, "paid-after-notice", &priced).await,
            Err(StoreError::SpendingPaused)
        ),
        "another won dispute cannot release an unresolved loss"
    );
    let free = quote(db, owner, &root, 0).await;
    db.submit(owner, "zero-cost", &free)
        .await
        .expect("zero reservation allowed");
    assert_eq!(
        notify(&router, "evt_four", "dp_one", &charge, "lost", false).await,
        204
    );
    assert!(
        api.process_dispute()
            .await
            .expect("actual merchant confirmed resolution")
    );
    let reopened = db
        .submit(owner, "paid-after-notice", &priced)
        .await
        .expect("paid spending resumes only after both restrictions clear");
    db.cancel_queued(owner, &reopened.job_id, "release-check")
        .await
        .expect("release verification job");
    assert_eq!(
        snapshots()
            .await
            .expect("no change to original settlement or wallet"),
        original
    );
    let owners:i64=sqlx::query_scalar("SELECT count(*) FROM test_dispute_work WHERE owner_id=$1 AND case_id IS NOT NULL AND NOT blocks_spend").bind(owner).fetch_one(db.pool()).await.expect("two distinct review cases");
    assert_eq!(owners, 2);
    let _ = stop.send(());
    server.await.expect("join");
}
