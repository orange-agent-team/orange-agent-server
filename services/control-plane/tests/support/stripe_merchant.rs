//! Isolated Stripe wire fixture shared by the checkout and cross-account marketplace journeys.
use axum::{
    Json, Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use orange_control_plane::payments::SandboxConfig;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
pub(crate) const MERCHANT: &str = "acct_development_test";
pub(crate) const SIGNING: &str = "whsec_local_only_development_fixture";
const API_KEY: &str = "sk_test_local_development_fixture";
pub(crate) fn config() -> SandboxConfig {
    SandboxConfig {
        merchant_id: MERCHANT.into(),
        api_key: API_KEY.into(),
        webhook_secret: SIGNING.into(),
    }
}
pub(crate) struct Provider {
    pub(crate) refunds: RefundMerchant,
    pub(crate) disputed_order: Mutex<Option<String>>,
    orders: Mutex<HashMap<String, (String, String)>>,
    pub(crate) posts: AtomicUsize,
    pub(crate) mode: AtomicUsize,
}
fn headers_ok(h: &HeaderMap) -> bool {
    use base64::{Engine, engine::general_purpose::STANDARD};
    h.get("stripe-version").and_then(|v| v.to_str().ok()) == Some("2026-11-18.endive")
        && h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Basic {}", STANDARD.encode(format!("{API_KEY}:"))))
}
fn session(order: &str, sid: &str, binding: &str, paid: bool, bad: bool) -> Value {
    let pi = format!("pi_{}", order.replace('-', ""));
    let ch = format!("ch_{}", order.replace('-', ""));
    let mut v = json!({"id":sid,"object":"checkout.session","livemode":false,"mode":"payment","client_reference_id":order,"metadata":{"orange_order_id":order,"orange_binding":binding},"currency":"usd","amount_total":500,
        "payment_method_types":["card"],"payment_status":if paid{"paid"}else{"unpaid"},"status":if paid{"complete"}else{"open"},
        "url":format!("https://checkout.stripe.com/c/pay/{sid}#test")});
    if paid {
        v["line_items"] = json!({"has_more":false,"data":[{"quantity":1,"currency":"usd","amount_total":500,"price":{"id":"price_devUnits","livemode":false,"unit_amount":500,"currency":"usd"}}]});
        v["payment_intent"] = json!({"id":pi,"object":"payment_intent","livemode":false,"amount":500,"amount_received":500,"currency":"usd","status":"succeeded","metadata":{"orange_order_id":order,"orange_binding":binding},
            "latest_charge":{"id":ch,"object":"charge","payment_intent":pi,"livemode":false,"paid":true,"captured":true,"refunded":bad,"disputed":false,"amount":500,"amount_captured":500,"amount_refunded":if bad{100}else{0},"currency":"usd"}});
    }
    v
}
async fn account(State(p): State<Arc<Provider>>, h: HeaderMap) -> impl IntoResponse {
    assert!(headers_ok(&h));
    Json(
        json!({"id":if p.mode.load(Ordering::SeqCst)==7{"acct_wrong"}else{MERCHANT},"object":"account"}),
    )
}
async fn create_session(
    State(p): State<Arc<Provider>>,
    h: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> axum::response::Response {
    assert!(headers_ok(&h));
    let order = &f["client_reference_id"];
    assert_eq!(
        h["idempotency-key"]
            .to_str()
            .expect("original idempotency key"),
        format!("orange-checkout-{order}")
    );
    assert_eq!(&f["metadata[orange_order_id]"], order);
    assert_eq!(&f["payment_intent_data[metadata][orange_order_id]"], order);
    assert_eq!(f["mode"], "payment");
    assert_eq!(f["line_items[0][price]"], "price_devUnits");
    assert_eq!(f["line_items[0][quantity]"], "1");
    assert_eq!(f["payment_method_types[0]"], "card");
    let sid = format!("cs_test_{}", order.replace('-', ""));
    let binding = &f["metadata[orange_binding]"];
    assert_eq!(binding.len(), 32);
    assert_eq!(&f["payment_intent_data[metadata][orange_binding]"], binding);
    p.orders
        .lock()
        .expect("store fixture session")
        .insert(sid.clone(), (order.clone(), binding.clone()));
    p.posts.fetch_add(1, Ordering::SeqCst);
    if p.mode.load(Ordering::SeqCst) == 1 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(session(order, &sid, binding, false, false)).into_response()
}
async fn retrieve(
    State(p): State<Arc<Provider>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&h));
    let (order, binding) = p
        .orders
        .lock()
        .expect("read fixture session")
        .get(&id)
        .expect("only known provider sessions")
        .clone();
    let mode = p.mode.load(Ordering::SeqCst);
    let mut result = session(&order, &id, &binding, true, mode == 2);
    match mode {
        3 => result["line_items"]["data"][0]["price"]["id"] = json!("price_other"),
        4 => result["metadata"]["orange_binding"] = json!("0".repeat(32)),
        5 => result["livemode"] = json!(true),
        6 => result["payment_intent"]["amount_received"] = json!(499),
        _ => {}
    }
    Json(result)
}
async fn dispute(
    State(p): State<Arc<Provider>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&headers));
    let order = p
        .disputed_order
        .lock()
        .expect("fixture dispute")
        .clone()
        .expect("registered charge");
    let suffix = order.replace('-', "");
    Json(
        json!({"id":id,"object":"dispute","livemode":false,"charge":format!("ch_{suffix}"),"payment_intent":format!("pi_{suffix}"),"amount":500,"currency":"usd","status":"won"}),
    )
}
#[derive(Default)]
pub(crate) struct RefundMerchant {
    pub posts: AtomicUsize,
    pub(crate) records: Mutex<HashMap<String, Value>>,
    pub(crate) lose: AtomicBool,
}
async fn charge(
    State(p): State<Arc<Provider>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&h));
    let suffix = id.strip_prefix("ch_").expect("test charge");
    let returned: i64 = p
        .refunds
        .records
        .lock()
        .expect("refund records")
        .values()
        .filter(|v| v["charge"] == id && v["status"] == "succeeded")
        .map(|v| v["amount"].as_i64().expect("minor"))
        .sum();
    Json(
        json!({"id":id,"object":"charge","payment_intent":format!("pi_{suffix}"),"livemode":false,"paid":true,"captured":true,"refunded":returned==500,"disputed":false,
        "amount":500,"amount_captured":500,"amount_refunded":returned,"currency":"usd"}),
    )
}
async fn create_refund(
    State(p): State<Arc<Provider>>,
    h: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> axum::response::Response {
    assert!(headers_ok(&h));
    let local = &f["metadata[orange_refund_id]"];
    let id = format!("re_{}", local.replace('-', ""));
    assert_eq!(
        h["idempotency-key"].to_str().expect("key"),
        format!("orange-refund-{local}")
    );
    let order = &f["metadata[orange_order_id]"];
    let charge = &f["charge"];
    assert_eq!(charge, &format!("ch_{}", order.replace('-', "")));
    let r = json!({"id":id,"object":"refund","amount":f["amount"].parse::<i64>().expect("minor"),"currency":"usd","charge":charge,"payment_intent":format!("pi_{}",order.replace('-',"")),
        "status":"pending","metadata":{"orange_refund_id":local,"orange_order_id":order,"orange_binding":f["metadata[orange_binding]"]}});
    assert!(
        p.refunds
            .records
            .lock()
            .expect("records")
            .insert(id, r.clone())
            .is_none(),
        "no second refund creation"
    );
    p.refunds.posts.fetch_add(1, Ordering::SeqCst);
    if p.refunds.lose.swap(false, Ordering::SeqCst) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    Json(r).into_response()
}
async fn retrieve_refund(
    State(p): State<Arc<Provider>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&h));
    Json(p.refunds.records.lock().expect("records")[&id].clone())
}
async fn list_refunds(
    State(p): State<Arc<Provider>>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&h));
    let results: Vec<_> = p
        .refunds
        .records
        .lock()
        .expect("records")
        .values()
        .filter(|v| v["charge"] == q["charge"])
        .cloned()
        .collect();
    Json(json!({"object":"list","data":results,"has_more":false}))
}
pub(crate) async fn start() -> (
    Arc<Provider>,
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let p = Arc::new(Provider {
        refunds: Default::default(),
        disputed_order: Mutex::new(None),
        orders: Mutex::new(HashMap::new()),
        posts: AtomicUsize::new(0),
        mode: AtomicUsize::new(0),
    });
    let provider = Router::new()
        .route("/v1/account", get(account))
        .route("/v1/checkout/sessions", post(create_session))
        .route("/v1/checkout/sessions/{id}", get(retrieve))
        .route("/v1/disputes/{id}", get(dispute))
        .route("/v1/charges/{id}", get(charge))
        .route("/v1/refunds", get(list_refunds).post(create_refund))
        .route("/v1/refunds/{id}", get(retrieve_refund))
        .with_state(p.clone());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = socket.local_addr().expect("loopback address");
    let (stop, ended) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(socket, provider)
            .with_graceful_shutdown(async {
                let _ = ended.await;
            })
            .await
            .expect("fixture server")
    });
    (p, addr, stop, server)
}
