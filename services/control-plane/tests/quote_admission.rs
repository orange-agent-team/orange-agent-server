//! Run against a fresh private PostgreSQL database; the delayed write is an explicit test fixture.
use orange_control_plane::store::{Store, StoreError};
use orange_protocol::{CreateBudget, InvocationPurpose, SubmitJob};
use uuid::Uuid;

async fn quote(db: &Store, owner: &str, budget: &str, seconds: i32) -> SubmitJob {
    let req = SubmitJob {
        quote_id: Uuid::new_v4().to_string(),
        invocation_id: Uuid::new_v4().to_string(),
        purpose: InvocationPurpose::Primary,
        root_budget_id: budget.into(),
        request_projection_digest: "e".repeat(64),
    };
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
        VALUES($1,$2,$3,$4,$5,'frozen-price',17,clock_timestamp()+$6::integer*interval '1 second')")
        .bind(&req.quote_id).bind(owner).bind(budget).bind(&req.invocation_id)
        .bind(&req.request_projection_digest).bind(seconds).execute(db.pool()).await.expect("issue quote");
    req
}

#[tokio::test]
async fn issued_terms_stay_immutable_and_only_new_effects_require_an_unexpired_quote() {
    let url = std::env::var("ORANGE_TEST_QUOTE_DATABASE_URL").expect("dedicated quote database");
    assert!(
        url.rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with("orange_quote_contract_"))
    );
    let db = Store::connect(&url).await.expect("connect");
    db.migrate().await.expect("apply migrations");
    let owner = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(db.pool())
        .await
        .expect("owner");
    sqlx::query("INSERT INTO wallets(owner_id,purchased) VALUES($1,100)")
        .bind(&owner)
        .execute(db.pool())
        .await
        .expect("development credits");
    let budget = db
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: "quote-test".into(),
                limit: "80".to_owned().try_into().expect("amount"),
            },
        )
        .await
        .expect("root")
        .root_budget_id;

    let req = quote(&db, &owner, &budget, 4).await;
    for change in [
        "max_amount=18",
        "price_version='another-price'",
        "expires_at=expires_at+interval '1 day'",
        "invocation_id='another-invocation'",
        "projection_digest=repeat('f',64)",
        "owner_id='another-owner'",
        "root_budget_id='another-root'",
        "quote_id='another-quote'",
    ] {
        let failure = sqlx::query(&format!("UPDATE quotes SET {change} WHERE quote_id=$1"))
            .bind(&req.quote_id)
            .execute(db.pool())
            .await
            .expect_err("original quote cannot change");
        assert_eq!(
            failure.as_database_error().and_then(|e| e.constraint()),
            Some("immutable_issued_quote_terms")
        );
    }
    let accepted = db.submit(&owner, "once", &req).await.expect("first effect");
    sqlx::query("SELECT pg_sleep_until(expires_at+interval '10 milliseconds') FROM quotes WHERE quote_id=$1")
        .bind(&req.quote_id).execute(db.pool()).await.expect("original quote expired");
    assert_eq!(
        db.submit(&owner, "once", &req)
            .await
            .expect("recover original effect")
            .job_id,
        accepted.job_id
    );
    let changed = SubmitJob {
        request_projection_digest: "b".repeat(64),
        ..req.clone()
    };
    assert!(matches!(
        db.submit(&owner, "once", &changed).await,
        Err(StoreError::Conflict)
    ));

    // Hold an otherwise valid insert until its exact database deadline. The
    // sequence proves the original insert was reached while the quote was live,
    // even when the transaction containing the Job and ledger later rolls back.
    sqlx::raw_sql("CREATE SEQUENCE test_quote_write_seen;
        CREATE FUNCTION test_delay_new_job() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE deadline timestamptz;
        BEGIN
            SELECT expires_at INTO STRICT deadline FROM quotes WHERE quote_id=NEW.quote_id;
            IF deadline <= clock_timestamp() THEN RAISE EXCEPTION 'fixture reached too late'; END IF;
            PERFORM nextval('test_quote_write_seen');
            PERFORM pg_sleep_until(deadline+interval '10 milliseconds');
            RETURN NEW;
        END $$;
        CREATE TRIGGER test_delay_job AFTER INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION test_delay_new_job();")
        .execute(db.pool()).await.expect("isolated delayed database effect");
    let late = quote(&db, &owner, &budget, 3).await;
    assert!(matches!(
        db.submit(&owner, "late", &late).await,
        Err(StoreError::QuoteExpired)
    ));
    let seen: bool = sqlx::query_scalar("SELECT is_called FROM test_quote_write_seen")
        .fetch_one(db.pool())
        .await
        .expect("sleep reached");
    assert!(seen);
    let held:(i64,i64,i64)=sqlx::query_as("SELECT w.purchased,w.reserved_purchased,b.reserved
        FROM wallets w JOIN budget_roots b USING(owner_id) WHERE w.owner_id=$1 AND b.root_budget_id=$2")
        .bind(&owner).bind(&budget).fetch_one(db.pool()).await.expect("holds");
    assert_eq!(held, (83, 17, 17));
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
        (SELECT count(*) FROM jobs WHERE owner_id=$1),
        (SELECT count(*) FROM ledger_transactions WHERE owner_id=$1),
        (SELECT count(*) FROM outbox WHERE owner_id=$1 AND event_type='job.queued'),
        (SELECT count(*) FROM idempotency WHERE owner_id=$1 AND operation='submit_job')",
    )
    .bind(&owner)
    .fetch_one(db.pool())
    .await
    .expect("single committed effect");
    assert_eq!(counts, (1, 1, 1, 1));
}
