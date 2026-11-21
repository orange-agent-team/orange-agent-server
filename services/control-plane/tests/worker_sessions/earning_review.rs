//! Real prior CPU marketplace settlements. Move only the review clock in the dedicated disposable DB.
use super::{Store, journey_admin};
use serde_json::{Value, json};
use sqlx::Row;

pub(super) async fn after_actual_jobs(
    db: &Store,
    buyer_a: &super::Buyer,
    buyer_b: &super::Buyer,
    app: &axum::Router,
    org: &str,
) {
    use orange_control_plane::provider_earnings::prepare_reviews;
    let alice = &buyer_a.owner[..];
    let bob = &buyer_b.owner[..];
    let income = "/api/v1/account/earnings";
    let initial = buyer_b.call(app, "GET", income, "", vec![], 200).await;
    assert_eq!(initial["user_id"], buyer_b.user);
    assert_eq!(initial["data"][0]["state"], "pending");
    assert_eq!(initial["data"].as_array().expect("own jobs").len(), 1);
    let pool = db.pool();
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("owned DB name");
    assert!(
        name.starts_with("orange_audio_grant_") && name.len() == "orange_audio_grant_".len() + 32
    );
    assert_eq!(prepare_reviews(pool, 100).await.expect("real clock"), 0);
    use orange_protocol::{CreateBudget, InvocationPurpose, SubmitJob};
    let budget = db
        .create_budget(
            bob,
            "spend-supply-credits",
            &CreateBudget {
                scope_id: "provider-spend".into(),
                limit: "1000".to_owned().try_into().expect("budget"),
            },
        )
        .await
        .expect("supplier budget");
    let demand = SubmitJob {
        root_budget_id: budget.root_budget_id,
        invocation_id: uuid::Uuid::new_v4().to_string(),
        quote_id: uuid::Uuid::new_v4().to_string(),
        request_projection_digest: "1".repeat(64),
        purpose: InvocationPurpose::Primary,
    };
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
       VALUES($1,$2,$3,$4,$5,'earning-test-quote',100,clock_timestamp()+interval '10 minutes')").bind(&demand.quote_id).bind(bob).bind(&demand.root_budget_id).bind(&demand.invocation_id).bind(&demand.request_projection_digest).execute(pool).await.expect("fixed demand quote fixture");
    assert!(matches!(
        db.submit(bob, "before-earning-release", &demand).await,
        Err(orange_control_plane::store::StoreError::Balance)
    ));
    let deferred:i64=sqlx::query_scalar("SELECT count(*) FROM provider_earning_status WHERE disposition='holding_period' AND units=1760")
        .fetch_one(pool).await.expect("before maturity");
    assert_eq!(deferred, 2);
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION provider_credit_now() RETURNS timestamptz LANGUAGE sql VOLATILE AS $$ SELECT clock_timestamp()+interval '24 hours' $$")
        .execute(pool).await.expect("advance only the test review clock");
    let (first, second) = tokio::join!(prepare_reviews(pool, 100), prepare_reviews(pool, 100));
    assert_eq!(first.expect("first scan") + second.expect("racing scan"), 2);
    let rows = sqlx::query("SELECT * FROM provider_earning_status ORDER BY job_id")
        .fetch_all(pool)
        .await
        .expect("original entitlements");
    assert_eq!(rows.len(), 2);
    let related = rows
        .iter()
        .find(|r| r.get::<&str, _>("buyer_owner") == org)
        .expect("buying team");
    assert_eq!(related.get::<&str, _>("provider_owner"), alice);
    assert_eq!(related.get::<&str, _>("disposition"), "related_trade");
    let normal = rows
        .iter()
        .find(|r| r.get::<&str, _>("provider_owner") == bob)
        .expect("independent supplier");
    assert_eq!(normal.get::<&str, _>("buyer_owner"), alice);
    assert_eq!(normal.get::<&str, _>("disposition"), "ready_for_review");
    let case = normal.get::<&str, _>("case_id");
    let job = normal.get::<&str, _>("job_id");
    let original = normal.get::<&str, _>("source_transaction");
    let maturity = buyer_b.call(app, "GET", income, "", vec![], 200).await;
    assert_eq!(maturity["data"][0]["state"], "review");
    assert_eq!(maturity["data"][0]["job_id"], job);
    buyer_a
        .call(
            app,
            "GET",
            &format!("{income}?after={job}"),
            "",
            vec![],
            422,
        )
        .await;
    buyer_b
        .call(
            app,
            "GET",
            &format!("{income}?owner_id={alice}"),
            "",
            vec![],
            422,
        )
        .await;

    let finance = journey_admin::Admin::new(db, bob).await;
    let other = journey_admin::Admin::new(db, alice).await;
    let path = |c: &str, owner: &str| {
        format!(
            "/admin/v1/reconciliation-cases/{c}/activity?environment=dev&scope_kind=tenant&scope_id={owner}"
        )
    };
    other
        .call(
            &other.maker,
            "GET",
            &path(case, alice),
            "",
            Value::Null,
            404,
        )
        .await;
    let support = journey_admin::Admin::with_role(db, bob, "support_operator").await;
    let (limited, _) = support
        .call(
            &support.maker,
            "GET",
            &path(case, bob),
            "",
            Value::Null,
            200,
        )
        .await;
    assert!(
        limited.get("earning").is_none(),
        "case visibility alone cannot disclose financial evidence"
    );
    let (preview, _) = finance
        .call(
            &finance.maker,
            "GET",
            &path(case, bob),
            "",
            Value::Null,
            200,
        )
        .await;
    assert_eq!(preview["earning"]["units"], "1760");
    assert_eq!(preview["earning"]["source_transaction"], original);
    let request = |case: &str, job: &str, owner: &str, version: &Value| {
        json!({"scope":{"environment":"dev","kind":"tenant","scope_id":owner},"target_version":version,
        "ticket_id":"earned-review","reason":"Verified original developer job settlement and source credits","action_type":"case.update",
        "payload":{"change":"release_earning","case_id":case,"job_id":job}})
    };
    let rcase = related.get::<&str, _>("case_id");
    let rjob = related.get::<&str, _>("job_id");
    let (blocked, _) = other
        .call(
            &other.maker,
            "GET",
            &path(rcase, alice),
            "",
            Value::Null,
            200,
        )
        .await;
    assert_eq!(blocked["earning"]["disposition"], "related_trade");
    other
        .call(
            &other.maker,
            "POST",
            "/admin/v1/actions",
            "",
            request(rcase, rjob, alice, &blocked["version"]),
            409,
        )
        .await;
    let (action, tag) = finance
        .call(
            &finance.maker,
            "POST",
            "/admin/v1/actions",
            "",
            request(case, job, bob, &preview["version"]),
            201,
        )
        .await;
    let base = format!(
        "/admin/v1/actions/{}",
        action["action_id"].as_str().expect("action id")
    );
    let approve = json!({"decision":"approve","payload_digest":action["payload_digest"],"policy_version":action["policy_version"],"reason":"Checked maturity and pending source transaction"});
    finance
        .call(
            &finance.maker,
            "POST",
            &format!("{base}/approvals"),
            &tag,
            approve.clone(),
            403,
        )
        .await;
    let (_, approved) = finance
        .call(
            &finance.checker,
            "POST",
            &format!("{base}/approvals"),
            &tag,
            approve,
            200,
        )
        .await;
    let execute = json!({"payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
    let node=sqlx::query("SELECT n.node_id,n.state FROM jobs j JOIN quote_execution_plans p USING(quote_id) JOIN nodes n ON n.node_id=p.target_node_id WHERE j.job_id=$1")
        .bind(job).fetch_one(pool).await.expect("original node");
    sqlx::query("UPDATE nodes SET state='quarantined' WHERE node_id=$1")
        .bind(node.get::<&str, _>("node_id"))
        .execute(pool)
        .await
        .expect("test node risk after approval");
    finance
        .call(
            &finance.maker,
            "POST",
            &format!("{base}/execute"),
            &approved,
            execute.clone(),
            409,
        )
        .await;
    let wallet = "SELECT earned,pending_earned FROM wallets WHERE owner_id=$1";
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(wallet)
            .bind(bob)
            .fetch_one(pool)
            .await
            .expect("frozen"),
        (0, 1760)
    );
    sqlx::query("UPDATE nodes SET state=$2 WHERE node_id=$1")
        .bind(node.get::<&str, _>("node_id"))
        .bind(node.get::<&str, _>("state"))
        .execute(pool)
        .await
        .expect("risk fixture cleared");
    // A current chargeback only blocks entitlements funded by the linked original lot.
    let injected=sqlx::query("INSERT INTO test_dispute_work(merchant_id,dispute_id,charge_id,order_id,owner_id,state,blocks_spend)
      SELECT o.merchant_id,'dp_earning_fixture',f.charge_id,a.order_id,o.owner_id,'unverified',true FROM test_job_lot_allocations a
      JOIN test_checkout_orders o USING(order_id) JOIN test_credit_fulfillments f USING(order_id) WHERE a.job_id=$1 AND a.charged>0 LIMIT 1")
      .bind(job).execute(pool).await.expect("test purchase dispute");
    assert_eq!(injected.rows_affected(), 1);
    let (risky, _) = finance
        .call(
            &finance.maker,
            "GET",
            &path(case, bob),
            "",
            Value::Null,
            200,
        )
        .await;
    assert_eq!(risky["earning"]["disposition"], "disputed_purchase");
    finance
        .call(
            &finance.maker,
            "POST",
            &format!("{base}/execute"),
            &approved,
            execute.clone(),
            409,
        )
        .await;
    sqlx::query("UPDATE test_dispute_work SET blocks_spend=false WHERE dispute_id='dp_earning_fixture' AND owner_id=$1")
       .bind(alice).execute(pool).await.expect("clear isolated fixture dispute");
    if let Ok(driver) = std::env::var("ORANGE_EARN_BROWSER") {
        crate::admin_browser::run(finance.app.clone(),&std::env::var("ORANGE_EARN_NODE").expect("node"),&driver,json!({"origin":finance.origin,
           "maker":finance.maker.token,"checker":finance.checker.token,"case":case,"job":job,"posting":original,"units":"1760","screenshot":std::env::var("ORANGE_EARN_SCREENSHOT").expect("capture")})).await;
        finance
            .call(
                &finance.maker,
                "POST",
                &format!("{base}/execute"),
                &approved,
                execute,
                412,
            )
            .await;
    } else {
        finance
            .call(
                &finance.maker,
                "POST",
                &format!("{base}/execute"),
                &approved,
                execute,
                202,
            )
            .await;
    }
    let (done, _) = finance
        .call(
            &finance.maker,
            "GET",
            &path(case, bob),
            "",
            Value::Null,
            200,
        )
        .await;
    assert_eq!(done["earning"]["disposition"], "released");
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(wallet)
            .bind(bob)
            .fetch_one(pool)
            .await
            .expect("available"),
        (1760, 0)
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(wallet)
            .bind(alice)
            .fetch_one(pool)
            .await
            .expect("related hold"),
        (0, 1760)
    );
    let entries: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account,amount FROM ledger_entries WHERE transaction_id=$1 ORDER BY account",
    )
    .bind(
        done["earning"]["release_transaction"]
            .as_str()
            .expect("release posting"),
    )
    .fetch_all(pool)
    .await
    .expect("balanced transfer");
    assert_eq!(
        entries,
        [
            (format!("{bob}:earned:available"), 1760),
            (format!("{bob}:earned:pending"), -1760)
        ]
    );
    assert_eq!(prepare_reviews(pool, 100).await.expect("retry scan"), 0);
    let spend = db
        .submit(bob, "after-earning-release", &demand)
        .await
        .expect("spend released contribution");
    let reserved: (i64, i64, i64) = sqlx::query_as(
        "SELECT earned,reserved_earned,pending_earned FROM wallets WHERE owner_id=$1",
    )
    .bind(bob)
    .fetch_one(pool)
    .await
    .expect("spend original earned bucket");
    assert_eq!(reserved, (1660, 100, 0));
    db.cancel_queued(bob, &spend.job_id, "cancel-development-demand")
        .await
        .expect("cancel queued spend");
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(wallet)
            .bind(bob)
            .fetch_one(pool)
            .await
            .expect("refund same earned bucket"),
        (1760, 0)
    );
    let personal = buyer_b.call(app, "GET", income, "", vec![], 200).await;
    assert_eq!(personal["data"][0]["state"], "available");
    assert_eq!(personal["data"][0]["units"], "1760");
    let held = buyer_a.call(app, "GET", income, "", vec![], 200).await;
    assert_eq!(held["data"][0]["state"], "review");
    assert_eq!(held["data"][0]["job_id"], rjob);
    for unsafe_key in ["buyer_owner", "case_id", "last_failure", "action_id"] {
        assert!(personal["data"][0].get(unsafe_key).is_none());
    }
    if let Ok(driver) = std::env::var("ORANGE_EARN_USER_BROWSER") {
        super::browser_flow(app,&driver,json!({"origin":buyer_b.origin,"token":buyer_b.token,"job":job,"foreign":rjob,
            "screenshot":std::env::var("ORANGE_EARN_USER_CAPTURE").expect("user evidence destination")})).await;
    }
}
