use super::admin_fixture::Admin;
use super::*;
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
