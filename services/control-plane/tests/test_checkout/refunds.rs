use super::*;
use axum::extract::Query;
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub(super) struct RefundMerchant {
    pub posts: AtomicUsize,
    records: Mutex<HashMap<String, Value>>,
    lose: AtomicBool,
}
pub(super) async fn charge(
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
pub(super) async fn create(
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
pub(super) async fn retrieve(
    State(p): State<Arc<Provider>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> impl IntoResponse {
    assert!(headers_ok(&h));
    Json(p.refunds.records.lock().expect("records")[&id].clone())
}
pub(super) async fn list(
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
struct Operator {
    id: String,
    token: String,
    csrf: String,
}
async fn operator(pool: &sqlx::PgPool, issuer: &str, owner: &str) -> Operator {
    use orange_control_plane::admin::{hash_secret, random_secret};
    let a = Operator {
        id: Uuid::new_v4().to_string(),
        token: random_secret(),
        csrf: random_secret(),
    };
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&a.id)
        .bind(issuer)
        .execute(pool)
        .await
        .expect("finance principal");
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'finance_operator','dev','tenant',$2,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)")
       .bind(&a.id).bind(owner).execute(pool).await.expect("finance grant");
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,floor(extract(epoch FROM clock_timestamp()))::bigint,floor(extract(epoch FROM clock_timestamp()))::bigint+600,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)")
      .bind(hash_secret(&a.token)).bind(&a.id).bind(&a.csrf).execute(pool).await.expect("explicit independent session");
    a
}
struct Admin {
    app: Router,
    origin: String,
    maker: Operator,
    checker: Operator,
}
impl Admin {
    async fn new(db: &Store, owner: &str) -> Self {
        use orange_control_plane::admin::{
            identity::{AdminIdentity, OidcConfig},
            routes,
        };
        let cfg: OidcConfig = serde_json::from_slice(
            &std::fs::read(
                std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("admin identity path"),
            )
            .expect("config"),
        )
        .expect("OIDC");
        let origin = cfg.admin_origin.clone();
        let maker = operator(db.pool(), &cfg.issuer, owner).await;
        let checker = operator(db.pool(), &cfg.issuer, owner).await;
        let app = routes::router(
            AdminIdentity::discover(db.pool().clone(), cfg)
                .await
                .expect("local admin OIDC"),
        );
        Self {
            app,
            origin,
            maker,
            checker,
        }
    }
    async fn call(
        &self,
        who: &Operator,
        method: &str,
        path: &str,
        etag: &str,
        body: Value,
        status: u16,
    ) -> (Value, String) {
        let request = Request::builder()
            .uri(path)
            .method(method)
            .header("cookie", format!("__Host-orange-admin={}", who.token))
            .header("origin", &self.origin)
            .header("x-csrf-token", &who.csrf)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .header("if-match", etag)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("Admin request");
        let res = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("Admin router");
        let code = res.status().as_u16();
        let etag = res
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let v: Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.expect("bounded"))
                .expect("response JSON");
        assert_eq!(code, status, "{path}: {v}");
        (v, etag)
    }
    async fn approve(&self, owner: &str, refund: &Value) {
        let case = refund["case_id"].as_str().expect("case");
        let (r,_)=self.call(&self.maker,"GET",&format!("/admin/v1/test-refunds/{case}?environment=dev&scope_kind=tenant&scope_id={owner}"),"",Value::Null,200).await;
        assert_eq!(r["held_microcredits"], refund["held_microcredits"]);
        let req = json!({"scope":{"environment":"dev","kind":"tenant","scope_id":owner},"target_version":r["target_version"],"ticket_id":"refund-review","reason":"Review original test purchase and refundable units","action_type":"payment.refund",
           "payload":{"payment_order_id":r["payment_order_id"],"amount":r["amount"],"reconciliation_case_id":case}});
        let mut wrong = req.clone();
        wrong["payload"]["amount"]["minor_units"] = json!("500");
        self.call(&self.maker, "POST", "/admin/v1/actions", "", wrong, 412)
            .await;
        let (action, tag) = self
            .call(&self.maker, "POST", "/admin/v1/actions", "", req, 201)
            .await;
        let id = action["action_id"].as_str().expect("action");
        let approval = json!({"decision":"approve","reason":"Independently verified the scoped order","payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
        let path = format!("/admin/v1/actions/{id}/approvals");
        self.call(&self.maker, "POST", &path, &tag, approval.clone(), 403)
            .await;
        let (_, new_tag) = self
            .call(&self.checker, "POST", &path, &tag, approval, 200)
            .await;
        let execute = json!({"payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
        let (accepted, _) = self
            .call(
                &self.maker,
                "POST",
                &format!("/admin/v1/actions/{id}/execute"),
                &new_tag,
                execute,
                202,
            )
            .await;
        assert_eq!(accepted["business_ref"], refund["refund_id"]);
    }
}
async fn due(db: &Store, id: &str) {
    sqlx::query("UPDATE test_refund_orders SET next_check=clock_timestamp() WHERE refund_id=$1 AND state NOT IN('succeeded','failed','cancelled')").bind(id).execute(db.pool()).await.expect("accelerate scheduling time only");
}
pub(super) async fn exercise(
    db: &Store,
    api: &Payments,
    app: &Router,
    b: &Buyer,
    origin: &str,
    provider: &Arc<Provider>,
    order: &str,
) {
    let admin = Admin::new(db, &b.owner).await;
    let base = format!("/api/v1/owners/{}", b.owner);
    let purchase = format!("{base}/checkout-orders/{order}");
    let path = format!("{purchase}/refunds");
    let root = db
        .create_budget(
            &b.owner,
            "refund-wallet",
            &orange_protocol::CreateBudget {
                scope_id: "refund-wallet".into(),
                limit: "9000".to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("budget")
        .root_budget_id;
    let job = job_from_wallet(db, &b.owner, &root, 4500).await;
    assert_eq!(
        user(
            (app, origin),
            b,
            "GET",
            &format!("{purchase}/refundable"),
            "",
            Value::Null,
            200
        )
        .await["max_refundable_minor"],
        "250"
    );
    let bad = Uuid::new_v4().to_string();
    user(
        (app, origin),
        b,
        "POST",
        &path,
        &bad,
        json!({"refund_id":bad,"currency":"usd","amount_minor":"251"}),
        409,
    )
    .await;
    db.cancel_queued(&b.owner, &job, "release-original")
        .await
        .expect("release actual original reservation");
    let start = wallet(db, &b.owner).await;
    for (step, amount, held) in [(0, 100, 1800), (1, 40, 720), (2, 10, 180)] {
        let id = Uuid::new_v4().to_string();
        let request = json!({"refund_id":id,"currency":"usd","amount_minor":amount.to_string()});
        let r = user((app, origin), b, "POST", &path, &id, request.clone(), 201).await;
        assert_eq!(
            user((app, origin), b, "POST", &path, &id, request, 201).await,
            r
        );
        assert_eq!(r["held_microcredits"], held.to_string());
        assert_eq!(r["state"], "held");
        let state = format!("{base}/refunds/{id}");
        let before = provider.refunds.posts.load(Ordering::SeqCst);
        assert!(!api.process_refund().await.expect("no unapproved dispatch"));
        if step == 0
            && let Ok(driver) = std::env::var("ORANGE_REFUND_ADMIN_BROWSER")
        {
            browser_flow(admin.app.clone(),&driver,json!({"origin":admin.origin,"maker":admin.maker.token,"checker":admin.checker.token,"case":r["case_id"],
                "refund":id,"order":order,"amount":amount.to_string(),"held":held.to_string(),"screenshot":std::env::var("ORANGE_REFUND_ADMIN_SCREENSHOT").expect("evidence")})).await;
            let distinct:bool=sqlx::query_scalar("SELECT maker_id=$2 AND checker_id=$3 AND maker_id<>checker_id FROM test_refund_authorizations WHERE refund_id=$1")
                .bind(&id).bind(&admin.maker.id).bind(&admin.checker.id).fetch_one(db.pool()).await.expect("independent DB confirmation");
            assert!(distinct);
        } else {
            admin.approve(&b.owner, &r).await;
        }

        if step == 2 {
            sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND role='finance_operator'").bind(&admin.checker.id).execute(db.pool()).await.expect("revoke checker before external dispatch");
            assert!(api.process_refund().await.expect("authority checked"));
            assert_eq!(
                user((app, origin), b, "GET", &state, "", Value::Null, 200).await["state"],
                "approval_lost"
            );
            assert_eq!(
                user(
                    (app, origin),
                    b,
                    "POST",
                    &format!("{state}/cancel"),
                    &id,
                    json!({}),
                    200
                )
                .await["state"],
                "cancelled"
            );
            assert_eq!(provider.refunds.posts.load(Ordering::SeqCst), before);
            continue;
        }
        provider.refunds.lose.store(step == 0, Ordering::SeqCst);
        let (x, y) = tokio::join!(api.process_refund(), api.process_refund());
        assert_eq!(
            u8::from(x.expect("first dispatcher")) + u8::from(y.expect("second dispatcher")),
            1
        );
        assert_eq!(provider.refunds.posts.load(Ordering::SeqCst), before + 1);
        if step == 0 {
            assert_eq!(
                user((app, origin), b, "GET", &state, "", Value::Null, 200).await["state"],
                "unknown"
            );
            user(
                (app, origin),
                b,
                "POST",
                &format!("{state}/cancel"),
                &id,
                json!({}),
                409,
            )
            .await;
            due(db, &id).await;
            assert!(
                api.process_refund()
                    .await
                    .expect("recover by original charge without resending")
            );
        }
        assert_eq!(
            user((app, origin), b, "GET", &state, "", Value::Null, 200).await["state"],
            "pending"
        );
        let rid = format!("re_{}", id.replace('-', ""));
        provider
            .refunds
            .records
            .lock()
            .expect("fixture refunds")
            .get_mut(&rid)
            .expect("only original")["status"] =
            json!(if step == 0 { "succeeded" } else { "failed" });
        let remote = provider.refunds.records.lock().expect("refund fixture")[&rid].clone();
        let event = json!({"id":format!("evt_refund_{}",id.replace('-',"")),"object":"event","livemode":false,"type":"refund.updated","data":{"object":remote}});
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(webhook(app, &event, SIGNING, ts, true).await, 403);
        let before_notify = wallet(db, &b.owner).await;
        for _ in 0..2 {
            assert_eq!(webhook(app, &event, SIGNING, ts, false).await, 204);
        }
        assert_eq!(
            wallet(db, &b.owner).await,
            before_notify,
            "event only schedules authoritative retrieval"
        );
        assert!(api.process_refund().await.expect("authoritative outcome"));
        assert_eq!(
            user((app, origin), b, "GET", &state, "", Value::Null, 200).await["state"],
            if step == 0 { "succeeded" } else { "failed" }
        );
        assert_eq!(provider.refunds.posts.load(Ordering::SeqCst), before + 1);
    }
    assert_eq!(
        wallet(db, &b.owner).await,
        (start.0, start.1, start.2 - 1800)
    );
    let balances: (i64, i64, i64) = sqlx::query_as(
        "SELECT available,refund_held,refunded FROM test_purchase_lots WHERE order_id=$1",
    )
    .bind(order)
    .fetch_one(db.pool())
    .await
    .expect("original purchase");
    assert_eq!(balances, (7200, 0, 1800));
    let evidence:(i64,i64)=sqlx::query_as("SELECT count(*),count(DISTINCT transaction_id) FROM test_refund_receipts JOIN test_refund_orders USING(refund_id) WHERE order_id=$1").bind(order).fetch_one(db.pool()).await.expect("single settlement per refund");
    assert_eq!(evidence, (3, 3));
}
