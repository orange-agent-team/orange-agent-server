use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use orange_control_plane::{
    http::{self, ApiState, DevIdentity},
    store::{Store, StoreError},
};
use orange_domain::Credits;
use orange_protocol::{CreateBudget, InvocationPurpose, Money, SubmitJob};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

fn money(n: i64) -> Money {
    Money(Credits::new(n).expect("fixture"))
}
async fn db() -> Store {
    let url = std::env::var("ORANGE_TEST_DATABASE_URL")
        .expect("real PostgreSQL URL is required; never skip this test");
    let store = Store::connect(&url).await.expect("connect PostgreSQL");
    store.migrate().await.expect("migrate");
    store
}
async fn owner(store: &Store, gift: i64, earned: i64, purchased: i64) -> String {
    let id = Uuid::new_v4().to_string();
    let mut tx = store.pool().begin().await.expect("begin");
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("owner");
    sqlx::query("INSERT INTO wallets(owner_id,gift,earned,purchased) VALUES($1,$2,$3,$4)")
        .bind(&id)
        .bind(gift)
        .bind(earned)
        .bind(purchased)
        .execute(&mut *tx)
        .await
        .expect("wallet");
    // Fixtures are isolated development balances, never recorded as production payments.
    tx.commit().await.expect("commit");
    id
}
async fn request(store: &Store, owner: &str, root: &str, max: i64) -> SubmitJob {
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let digest = "a".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'test-price-v1',$6,clock_timestamp()+interval '5 minutes')")
        .bind(&quote).bind(owner).bind(root).bind(&invocation).bind(&digest).bind(max).execute(store.pool()).await.expect("test quote");
    SubmitJob {
        root_budget_id: root.into(),
        invocation_id: invocation,
        purpose: InvocationPurpose::Primary,
        quote_id: quote,
        request_projection_digest: digest,
    }
}

#[tokio::test]
async fn concurrent_budget_contention_is_atomic_and_cancel_restores_original_buckets() {
    let store = db().await;
    let who = owner(&store, 2000, 3000, 15000).await;
    let root = store
        .create_budget(
            &who,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(10000),
            },
        )
        .await
        .expect("root");
    let a = request(&store, &who, &root.root_budget_id, 6000).await;
    let b = request(&store, &who, &root.root_budget_id, 6000).await;
    let (ra, rb) = tokio::join!(store.submit(&who, "a", &a), store.submit(&who, "b", &b));
    let job = match (ra, rb) {
        (Ok(job), Err(StoreError::Budget)) | (Err(StoreError::Budget), Ok(job)) => job,
        other => panic!("unexpected {other:?}"),
    };
    let wallet = sqlx::query("SELECT * FROM wallets WHERE owner_id=$1")
        .bind(&who)
        .fetch_one(store.pool())
        .await
        .expect("wallet");
    assert_eq!(
        (
            wallet.get::<i64, _>("gift"),
            wallet.get::<i64, _>("earned"),
            wallet.get::<i64, _>("purchased")
        ),
        (0, 0, 14000)
    );
    assert_eq!(
        store
            .budget(&who, &root.root_budget_id)
            .await
            .expect("budget")
            .reserved,
        money(6000)
    );
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE owner_id=$1")
        .bind(&who)
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(before, 1);
    let (c1, c2) = tokio::join!(
        store.cancel_queued(&who, &job.job_id, "cancel"),
        store.cancel_queued(&who, &job.job_id, "cancel")
    );
    assert_eq!(c1.expect("cancel one").charge, Some(money(0)));
    assert_eq!(c2.expect("cancel two").charge, Some(money(0)));
    let w = sqlx::query("SELECT * FROM wallets WHERE owner_id=$1")
        .bind(&who)
        .fetch_one(store.pool())
        .await
        .expect("wallet");
    assert_eq!(
        (
            w.get::<i64, _>("gift"),
            w.get::<i64, _>("earned"),
            w.get::<i64, _>("purchased")
        ),
        (2000, 3000, 15000)
    );
    assert_eq!(
        store
            .budget(&who, &root.root_budget_id)
            .await
            .expect("budget")
            .reserved,
        money(0)
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE owner_id=$1")
            .bind(&who)
            .fetch_one(store.pool())
            .await
            .expect("ledger");
    assert_eq!(ledger, 2);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox WHERE owner_id=$1")
        .bind(&who)
        .fetch_one(store.pool())
        .await
        .expect("outbox");
    assert_eq!(events, 3);
}

#[tokio::test]
async fn idempotency_checks_content_and_current_authority() {
    let store = db().await;
    let who = owner(&store, 0, 0, 10000).await;
    let create = CreateBudget {
        scope_id: "run".into(),
        limit: money(10000),
    };
    let root = store
        .create_budget(&who, "root", &create)
        .await
        .expect("create");
    assert_eq!(
        store
            .create_budget(&who, "root", &create)
            .await
            .expect("retry")
            .root_budget_id,
        root.root_budget_id
    );
    assert!(matches!(
        store
            .create_budget(
                &who,
                "root",
                &CreateBudget {
                    limit: money(9999),
                    ..create
                }
            )
            .await,
        Err(StoreError::Conflict)
    ));
    let req = request(&store, &who, &root.root_budget_id, 3000).await;
    let (one, two) = tokio::join!(
        store.submit(&who, "same", &req),
        store.submit(&who, "same", &req)
    );
    let a = one.expect("one");
    assert_eq!(a.job_id, two.expect("two").job_id);
    assert_eq!(
        a.job_id,
        store
            .submit(&who, "different-transport-key", &req)
            .await
            .expect("same invocation")
            .job_id
    );
    let changed = SubmitJob {
        purpose: InvocationPurpose::Summary,
        ..req.clone()
    };
    assert!(matches!(
        store.submit(&who, "same", &changed).await,
        Err(StoreError::Conflict)
    ));
    sqlx::query("UPDATE owners SET state='restricted' WHERE owner_id=$1")
        .bind(&who)
        .execute(store.pool())
        .await
        .expect("revoke");
    assert!(matches!(
        store.submit(&who, "same", &req).await,
        Err(StoreError::Forbidden)
    ));
    assert!(matches!(
        store.job(&who, &a.job_id).await,
        Err(StoreError::Forbidden)
    ));
}

#[tokio::test]
async fn failed_freeze_and_bad_quotes_leave_no_side_effect() {
    let store = db().await;
    let who = owner(&store, 10, 10, 10).await;
    let root = store
        .create_budget(
            &who,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(5000),
            },
        )
        .await
        .expect("root");
    let req = request(&store, &who, &root.root_budget_id, 1000).await;
    assert!(matches!(
        store.submit(&who, "no-money", &req).await,
        Err(StoreError::Balance)
    ));
    assert_eq!(
        store
            .budget(&who, &root.root_budget_id)
            .await
            .expect("root")
            .reserved,
        money(0)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE owner_id=$1")
        .bind(&who)
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(count, 0);
    let wrong = SubmitJob {
        request_projection_digest: "b".repeat(64),
        ..req.clone()
    };
    assert!(matches!(
        store.submit(&who, "bad-digest", &wrong).await,
        Err(StoreError::QuoteMismatch)
    ));
    sqlx::query(
        "UPDATE quotes SET expires_at=clock_timestamp()-interval '1 second' WHERE quote_id=$1",
    )
    .bind(&req.quote_id)
    .execute(store.pool())
    .await
    .expect("expire");
    assert!(matches!(
        store.submit(&who, "expired", &req).await,
        Err(StoreError::QuoteExpired)
    ));
}

#[tokio::test]
async fn database_enforces_immutable_balanced_ledger() {
    let store = db().await;
    let who = owner(&store, 0, 0, 1000).await;
    let root = store
        .create_budget(
            &who,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(1000),
            },
        )
        .await
        .expect("root");
    let req = request(&store, &who, &root.root_budget_id, 500).await;
    store.submit(&who, "job", &req).await.expect("job");
    assert!(sqlx::query("UPDATE ledger_entries SET amount=0 WHERE transaction_id IN (SELECT transaction_id FROM ledger_transactions WHERE owner_id=$1)").bind(&who).execute(store.pool()).await.is_err());
    let mut tx = store.pool().begin().await.expect("begin");
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&id)
    .bind(&id)
    .bind(&who)
    .execute(&mut *tx)
    .await
    .expect("transaction");
    sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,'invalid',7)")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("entry deferred");
    assert!(tx.commit().await.is_err());
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ledger_transactions WHERE transaction_id=$1)",
    )
    .bind(&id)
    .fetch_one(store.pool())
    .await
    .expect("exists");
    assert!(!exists);
}

#[tokio::test]
async fn http_rejects_unauthenticated_cross_owner_and_unknown_fields() {
    let store = db().await;
    let a = owner(&store, 0, 0, 1000).await;
    let b = owner(&store, 0, 0, 1000).await;
    let root = store
        .create_budget(
            &a,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(1000),
            },
        )
        .await
        .expect("root");
    let token = "b".repeat(48);
    let app = http::router(
        ApiState::development(
            store,
            vec![DevIdentity {
                owner_id: b,
                token: token.clone(),
            }],
        )
        .expect("state"),
    );
    let url = format!("/api/v1/budgets/{}", root.root_budget_id);
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(&url)
                    .body(Body::empty())
                    .expect("request")
            )
            .await
            .expect("response")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(&url)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request")
            )
            .await
            .expect("response")
            .status(),
        StatusCode::NOT_FOUND
    );
    let bad = Request::builder()
        .method("POST")
        .uri("/api/v1/budgets")
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", "key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"scope_id":"x","limit":"1","actor":"root"}"#))
        .expect("request");
    assert_eq!(
        app.oneshot(bad).await.expect("response").status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn resource_catalogue_resolves_each_business_owner_on_the_migrated_database() {
    use serde::Deserialize;
    use std::collections::BTreeSet;
    #[derive(Deserialize)]
    struct Edge {
        column: String,
        target: String,
        target_key: String,
    }
    #[derive(Deserialize)]
    struct Subject {
        kind: String,
        identity_table: String,
        credential: String,
        owner_path: Vec<Edge>,
    }
    #[derive(Deserialize)]
    struct Resource {
        resource: String,
        table: String,
        authority: String,
        owner_path: Vec<Edge>,
    }
    #[derive(Deserialize)]
    struct Catalogue {
        version: u8,
        subjects: Vec<Subject>,
        resources: Vec<Resource>,
    }
    async fn resolve(db: &Store, start: &str, path: &[Edge]) {
        let mut table = start;
        for e in path {
            let linked:bool=sqlx::query_scalar("SELECT EXISTS(
                SELECT 1 FROM pg_constraint fk
                  JOIN pg_attribute c ON c.attrelid=fk.conrelid AND c.attnum=fk.conkey[1]
                  JOIN pg_attribute t ON t.attrelid=fk.confrelid AND t.attnum=fk.confkey[1]
                WHERE fk.contype='f' AND fk.convalidated AND array_length(fk.conkey,1)=1 AND array_length(fk.confkey,1)=1
                  AND fk.conrelid=to_regclass($1) AND fk.confrelid=to_regclass($2) AND c.attname=$3 AND t.attname=$4 AND c.attnotnull AND t.attnotnull)")
                .bind(table).bind(&e.target).bind(&e.column).bind(&e.target_key).fetch_one(db.pool()).await.expect("PostgreSQL catalog");
            assert!(
                linked,
                "{table}.{} must reference {}.{} through a required foreign key",
                e.column, e.target, e.target_key
            );
            table = &e.target;
        }
        assert_eq!(
            table, "owners",
            "owner path must end at one business account"
        );
    }
    let db = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("real PostgreSQL"))
        .await
        .expect("store");
    db.migrate().await.expect("migrated schema");
    let c: Catalogue = serde_json::from_slice(include_bytes!(
        "../../../docs/ownership/subjects-and-resources.json"
    ))
    .expect("published catalogue");
    assert_eq!(c.version, 1);
    assert_eq!(
        c.subjects
            .iter()
            .map(|a| a.kind.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "personal_user",
            "organization",
            "provider_device",
            "platform_operator"
        ])
    );
    for subject in &c.subjects {
        assert!(!subject.credential.is_empty());
        if subject.kind == "platform_operator" {
            assert_eq!(subject.identity_table, "admin_principals");
            assert!(subject.owner_path.is_empty());
        } else {
            resolve(&db, &subject.identity_table, &subject.owner_path).await;
        }
    }
    let names = c
        .resources
        .iter()
        .map(|r| r.resource.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), c.resources.len());
    for required in [
        "job",
        "artifact",
        "wallet",
        "budget",
        "quote",
        "node",
        "job_result",
        "model_execution_profile",
        "organization_resource",
        "input_credential",
        "cloud_workspace",
        "cloud_run",
        "ledger_transaction",
        "reconciliation_case",
    ] {
        assert!(names.contains(required), "missing {required}");
    }
    for r in &c.resources {
        if r.authority == "account" {
            assert!(!r.owner_path.is_empty());
            resolve(&db, &r.table, &r.owner_path).await;
        } else {
            assert_eq!(r.authority, "platform");
            assert_eq!(r.table, "reviewed_execution_profiles");
            assert!(r.owner_path.is_empty());
            let identity:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_constraint k JOIN pg_attribute a ON a.attrelid=k.conrelid AND a.attnum=k.conkey[1] WHERE k.contype='p' AND k.conrelid=to_regclass($1) AND cardinality(k.conkey)=1 AND a.attname='execution_profile_id') AND 3=(SELECT count(*) FROM pg_attribute WHERE attrelid=to_regclass($1) AND attname IN ('model_sha256','execution_profile_digest','enabled') AND attnotnull)").bind(&r.table).fetch_one(db.pool()).await.expect("review identity");
            assert!(identity);
        }
    }
}
