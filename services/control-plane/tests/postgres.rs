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
    request_for_seconds(store, owner, root, max, 300).await
}
async fn request_for_seconds(
    store: &Store,
    owner: &str,
    root: &str,
    max: i64,
    seconds: i32,
) -> SubmitJob {
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let digest = "a".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'test-price-v1',$6,clock_timestamp()+($7::integer * interval '1 second'))")
        .bind(&quote).bind(owner).bind(root).bind(&invocation).bind(&digest).bind(max).bind(seconds).execute(store.pool()).await.expect("test quote");
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
    let expired = request_for_seconds(&store, &who, &root.root_budget_id, 1000, -1).await;
    assert!(matches!(
        store.submit(&who, "expired", &expired).await,
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

#[tokio::test]
async fn domain_entity_references_match_owned_primary_keys_and_validated_foreign_keys() {
    use serde::Deserialize;
    use std::collections::{BTreeMap, BTreeSet};
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entity {
        table: String,
        primary_key: Vec<String>,
        reference: Vec<String>,
        authority: String,
        relations: BTreeMap<String, String>,
        lifetime: String,
    }
    #[derive(Deserialize)]
    struct Catalog {
        entities: BTreeMap<String, Entity>,
    }
    let catalogue: Catalog =
        serde_json::from_slice(include_bytes!("../../../docs/entities/catalogue.json"))
            .expect("typed catalogue");
    assert_eq!(
        catalogue
            .entities
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "User",
            "Organization",
            "Node",
            "ModelVersion",
            "Job",
            "Attempt",
            "Artifact",
            "Ledger"
        ])
    );
    let store = db().await;
    for (name, entity) in catalogue.entities {
        assert!(
            !entity.authority.is_empty()
                && !entity.lifetime.is_empty()
                && !entity.reference.is_empty()
        );
        let primary:Vec<String>=sqlx::query_scalar("SELECT a.attname::text FROM pg_index i CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(attnum,pos) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.attnum WHERE i.indisprimary AND i.indrelid=to_regclass($1) ORDER BY k.pos").bind(&entity.table).fetch_all(store.pool()).await.expect("primary key");
        assert_eq!(primary, entity.primary_key, "{name}");
        for column in &entity.reference {
            let required:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid=to_regclass($1) AND attname=$2 AND NOT attisdropped AND attnotnull)").bind(&entity.table).bind(column).fetch_one(store.pool()).await.expect("reference column");
            assert!(required, "{name}.{column} must exist and be required");
        }
        for (column, parent) in entity.relations {
            let (parent_table, parent_column) = parent.split_once('.').expect("qualified target");
            let validated:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_constraint fk JOIN pg_attribute a ON a.attrelid=fk.conrelid AND a.attnum=fk.conkey[1] JOIN pg_attribute b ON b.attrelid=fk.confrelid AND b.attnum=fk.confkey[1] WHERE fk.contype='f' AND fk.convalidated AND cardinality(fk.conkey)=1 AND fk.conrelid=to_regclass($1) AND a.attname=$2 AND fk.confrelid=to_regclass($3) AND b.attname=$4)").bind(&entity.table).bind(&column).bind(parent_table).bind(parent_column).fetch_one(store.pool()).await.expect("ownership FK");
            assert!(validated, "{name}.{column} -> {parent}");
        }
    }
}

#[tokio::test]
async fn missing_owner_and_invalid_wire_amounts_create_no_budget_or_postings() {
    let store = db().await;
    let payer = owner(&store, 100, 0, 0).await;
    let secret = Uuid::new_v4().simple().to_string();
    let app = http::router(
        ApiState::development(
            store.clone(),
            vec![DevIdentity {
                owner_id: payer.clone(),
                token: secret.clone(),
            }],
        )
        .expect("development identity"),
    );
    let req = CreateBudget {
        scope_id: "scope".into(),
        limit: money(10),
    };
    assert!(matches!(
        store.create_budget("", "missing-owner", &req).await,
        Err(StoreError::Invalid)
    ));
    for amount in [
        serde_json::json!("-1"),
        serde_json::json!("0.7"),
        serde_json::json!("07"),
        serde_json::json!(7),
    ] {
        let payload = serde_json::json!({"scope_id":"scope","limit":amount});
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/budgets")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {secret}"))
                    .header("idempotency-key", "bad-money")
                    .body(Body::from(payload.to_string()))
                    .expect("HTTP request"),
            )
            .await
            .expect("real router");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM budget_roots WHERE owner_id=$1),(SELECT COUNT(*) FROM ledger_transactions WHERE owner_id=$1)").bind(&payer).fetch_one(store.pool()).await.expect("no invalid writes");
    assert_eq!(counts, (0, 0));
}

#[tokio::test]
async fn only_one_concurrent_terminal_revision_wins_and_an_old_attempt_cannot_rewind_it() {
    let store = db().await;
    let owner = owner(&store, 0, 0, 20000).await;
    let root = store
        .create_budget(
            &owner,
            "revision-budget",
            &CreateBudget {
                scope_id: "revision-race".into(),
                limit: money(8000),
            },
        )
        .await
        .expect("budget");
    let req = request(&store, &owner, &root.root_budget_id, 3000).await;
    let job = store
        .submit(&owner, "revision-job", &req)
        .await
        .expect("job")
        .job_id;
    assert_eq!(sqlx::query_scalar::<_,i64>("UPDATE jobs SET execution_state=execution_state WHERE job_id=$1 RETURNING state_revision").bind(&job).fetch_one(store.pool()).await.expect("unchanged"),1);
    let running:i64=sqlx::query_scalar("UPDATE jobs SET execution_state='running' WHERE job_id=$1 AND state_revision=1 RETURNING state_revision").bind(&job).fetch_one(store.pool()).await.expect("start");
    assert_eq!(running, 2);
    async fn terminal(pool: &sqlx::PgPool, job: &str, version: i64, state: &str) -> u64 {
        sqlx::query("UPDATE jobs SET execution_state=$3 WHERE job_id=$1 AND state_revision=$2 AND execution_state='running'")
          .bind(job).bind(version).bind(state).execute(pool).await.expect("competing finalization").rows_affected()
    }
    let (a, b) = tokio::join!(
        terminal(store.pool(), &job, running, "cancelled"),
        terminal(store.pool(), &job, running, "succeeded")
    );
    assert_eq!(a + b, 1);
    let (state, version): (String, i64) =
        sqlx::query_as("SELECT execution_state,state_revision FROM jobs WHERE job_id=$1")
            .bind(&job)
            .fetch_one(store.pool())
            .await
            .expect("result");
    assert_eq!(state, if a == 1 { "cancelled" } else { "succeeded" });
    assert_eq!(version, 3);
    for sql in [
        "UPDATE jobs SET execution_state='queued' WHERE job_id=$1",
        "UPDATE jobs SET state_revision=1 WHERE job_id=$1",
    ] {
        let err = sqlx::query(sql)
            .bind(&job)
            .execute(store.pool())
            .await
            .expect_err("state fence");
        assert_eq!(
            err.as_database_error().and_then(|x| x.code()).as_deref(),
            Some("23514")
        );
    }
    let current: (String, i64) =
        sqlx::query_as("SELECT execution_state,state_revision FROM jobs WHERE job_id=$1")
            .bind(&job)
            .fetch_one(store.pool())
            .await
            .expect("terminal fact");
    assert_eq!(current, (state, version));
}

#[tokio::test]
async fn existing_schema66_job_and_hold_survive_revision_upgrade() {
    let url =
        std::env::var("ORANGE_TEST_UPGRADE_DATABASE_URL").expect("dedicated empty upgrade DB");
    let store = Store::connect(&url).await.expect("connect");
    let missing: bool = sqlx::query_scalar("SELECT to_regclass('public.jobs') IS NULL")
        .fetch_one(store.pool())
        .await
        .expect("fresh DB");
    assert!(missing, "dedicated empty upgrade database required");
    let stage = tempfile::tempdir().expect("old migrations");
    let sources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut n = 0;
    for item in std::fs::read_dir(sources).expect("migration sources") {
        let entry = item.expect("file");
        let name = entry.file_name();
        let text = name.to_str().expect("filename");
        if text.ends_with(".sql")
            && text
                .split('_')
                .next()
                .expect("prefix")
                .parse::<u32>()
                .expect("version")
                <= 66
        {
            std::fs::copy(entry.path(), stage.path().join(name)).expect("copy immutable migration");
            n += 1;
        }
    }
    assert_eq!(n, 66);
    sqlx::migrate::Migrator::new(stage.path())
        .await
        .expect("legacy migration chain")
        .run(store.pool())
        .await
        .expect("old schema");
    let who = owner(&store, 40, 160, 300).await;
    let budget = store
        .create_budget(
            &who,
            "legacy-budget",
            &CreateBudget {
                scope_id: "old-scope".into(),
                limit: money(500),
            },
        )
        .await
        .expect("old budget");
    let req = request(&store, &who, &budget.root_budget_id, 250).await;
    let original = store
        .submit(&who, "old-submit", &req)
        .await
        .expect("frozen on schema 66");
    let account_snapshot = || {
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT gift,earned,purchased FROM wallets WHERE owner_id=$1",
        )
        .bind(&who)
        .fetch_one(store.pool())
    };
    let old_wallet = account_snapshot().await.expect("old wallet");
    store.migrate().await.expect("upgrade in place");
    let version: i64 = sqlx::query_scalar("SELECT state_revision FROM jobs WHERE job_id=$1")
        .bind(&original.job_id)
        .fetch_one(store.pool())
        .await
        .expect("initial revision");
    assert_eq!(version, 1);
    assert_eq!(account_snapshot().await.expect("wallet"), old_wallet);
    let replay = store
        .submit(&who, "old-submit", &req)
        .await
        .expect("prior key");
    assert_eq!(replay.job_id, original.job_id);
    assert_eq!(replay.reserved.0.units(), 250);
    let closed = store
        .cancel_queued(&who, &replay.job_id, "legacy-cancel")
        .await
        .expect("unconditional older client");
    assert_eq!(closed.execution_state, "cancelled");
    assert_eq!(
        account_snapshot().await.expect("restitution"),
        (40, 160, 300)
    );
}
