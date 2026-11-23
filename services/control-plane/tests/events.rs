use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use orange_control_plane::{
    events::{EventError, EventStore},
    http::{self, ApiState, DevIdentity},
    store::Store,
};
use orange_domain::Credits;
use orange_protocol::{CreateBudget, InvocationPurpose, Money, SubmitJob};
use tower::ServiceExt;
use uuid::Uuid;
fn money(n: i64) -> Money {
    Money(Credits::new(n).expect("amount"))
}
async fn fixture() -> (Store, EventStore, String, String) {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("actual PostgreSQL required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let owner = Uuid::new_v4().to_string();
    let other = Uuid::new_v4().to_string();
    for id in [&owner, &other] {
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
            .bind(id)
            .execute(store.pool())
            .await
            .expect("owner fixture");
        sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,100)")
            .bind(id)
            .execute(store.pool())
            .await
            .expect("funding fixture");
    }
    let events = EventStore::new(store.pool().clone());
    (store, events, owner, other)
}
async fn quote(store: &Store, owner: &str, root: &str, maximum: i64) -> SubmitJob {
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'event-fixture-price',$6,clock_timestamp()+interval '5 minutes')").bind(&quote).bind(owner).bind(root).bind(&invocation).bind("a".repeat(64)).bind(maximum).execute(store.pool()).await.expect("quote fixture");
    SubmitJob {
        root_budget_id: root.into(),
        invocation_id: invocation,
        purpose: InvocationPurpose::Primary,
        quote_id: quote,
        request_projection_digest: "a".repeat(64),
    }
}
#[tokio::test]
async fn domain_commit_produces_ordered_replay_and_duplicates_do_not_add_events() {
    let (store, events, owner, other) = fixture().await;
    let root = store
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(50),
            },
        )
        .await
        .expect("root");
    let request = quote(&store, &owner, &root.root_budget_id, 10).await;
    let job = store.submit(&owner, "submit", &request).await.expect("job");
    let stream = events
        .discover(&owner, "job", &job.job_id)
        .await
        .expect("stream");
    let (a, b) = tokio::join!(
        store.cancel_queued(&owner, &job.job_id, "cancel"),
        store.cancel_queued(&owner, &job.job_id, "cancel")
    );
    a.expect("cancel");
    b.expect("duplicate cancellation");
    let first = events
        .read(&owner, &stream.stream_id, "0", 1)
        .await
        .expect("first event");
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].seq, "1");
    assert_eq!(first.events[0].event_type, "job.queued");
    let last = events
        .read(&owner, &stream.stream_id, &first.next_cursor, 50)
        .await
        .expect("reconnect");
    assert_eq!(last.events.len(), 1);
    assert_eq!(last.events[0].seq, "2");
    assert_eq!(last.events[0].event_type, "job.cancelled");
    let mismatches:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN outbox o ON o.domain_event_id=e.event_id WHERE e.payload_digest!=encode(sha256(convert_to(o.payload,'UTF8')),'hex') OR e.source_outbox_id!=o.event_id").fetch_one(store.pool()).await.expect("independent database digest verification");
    assert_eq!(mismatches, 0);
    assert!(
        events
            .read(&owner, &stream.stream_id, &last.next_cursor, 50)
            .await
            .expect("caught up")
            .events
            .is_empty()
    );
    assert!(matches!(
        events.read(&other, &stream.stream_id, "0", 50).await,
        Err(EventError::NotFound)
    ));
    for bad in ["-1", "01", "9223372036854775808", "3"] {
        assert!(matches!(
            events.read(&owner, &stream.stream_id, bad, 50).await,
            Err(EventError::Invalid)
        ));
    }
    assert!(
        sqlx::query("UPDATE durable_events SET event_type='tampered' WHERE stream_id=$1")
            .bind(&stream.stream_id)
            .execute(store.pool())
            .await
            .is_err()
    );
    sqlx::query("UPDATE owners SET state='restricted' WHERE owner_id=$1")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("revoke owner");
    assert!(matches!(
        events.read(&owner, &stream.stream_id, "0", 50).await,
        Err(EventError::Forbidden)
    ));
}
#[tokio::test]
async fn failed_domain_transaction_publishes_no_event_and_cursor_retention_is_explicit() {
    let (store, events, owner, _) = fixture().await;
    let root = store
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(5),
            },
        )
        .await
        .expect("root");
    let request = quote(&store, &owner, &root.root_budget_id, 10).await;
    assert!(store.submit(&owner, "over-budget", &request).await.is_err());
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.owner_id=$1").bind(&owner).fetch_one(store.pool()).await.expect("count");
    assert_eq!(count, 1);
    let stream = events
        .discover(&owner, "budget", &root.root_budget_id)
        .await
        .expect("budget stream");
    assert!(matches!(
        events.prune_delivered(&stream.stream_id, 1).await,
        Err(EventError::Undelivered)
    ));
    // Explicit delivered fixture tests retention only; this is not an outbox transport assertion.
    sqlx::query("UPDATE outbox SET delivered_at=clock_timestamp() WHERE owner_id=$1")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("delivered fixture");
    events
        .prune_delivered(&stream.stream_id, 1)
        .await
        .expect("retention");
    assert!(matches!(
        events.read(&owner, &stream.stream_id, "0", 50).await,
        Err(EventError::SnapshotRequired)
    ));
    assert!(
        events
            .read(&owner, &stream.stream_id, "1", 50)
            .await
            .expect("retained cursor")
            .events
            .is_empty()
    );
    let replay = store
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(5),
            },
        )
        .await
        .expect("business key survives event retention");
    assert_eq!(replay.root_budget_id, root.root_budget_id);
}
#[tokio::test]
async fn actual_http_router_requires_identity_and_owner_for_event_replay() {
    let (store, events, owner, other) = fixture().await;
    let root = store
        .create_budget(
            &owner,
            "root",
            &CreateBudget {
                scope_id: "run".into(),
                limit: money(5),
            },
        )
        .await
        .expect("root");
    let stream = events
        .discover(&owner, "budget", &root.root_budget_id)
        .await
        .expect("stream");
    let token = Uuid::new_v4().to_string();
    let other_token = Uuid::new_v4().to_string();
    let app = http::router(
        ApiState::development(
            store,
            vec![
                DevIdentity {
                    owner_id: owner,
                    token: token.clone(),
                },
                DevIdentity {
                    owner_id: other,
                    token: other_token.clone(),
                },
            ],
        )
        .expect("explicit development identities"),
    );
    let path = format!("/api/v1/event-streams/{}?after=0", stream.stream_id);
    for (credential, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(other_token), StatusCode::NOT_FOUND),
        (Some(token), StatusCode::OK),
    ] {
        let mut request = Request::builder().uri(&path);
        if let Some(token) = credential {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("router");
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        if expected == StatusCode::OK {
            let body = to_bytes(response.into_body(), 65536).await.expect("body");
            let page: orange_protocol::events::EventPage =
                serde_json::from_slice(&body).expect("typed event page");
            assert_eq!(page.events[0].seq, "1");
            assert_eq!(page.next_cursor, "1");
        }
    }
}

async fn retained_stream(store: &Store, owner: &str, length: i64, pending: i64) -> String {
    let entity = Uuid::new_v4().to_string();
    let mut tx = store.pool().begin().await.expect("fixture transaction");
    let stream:String=sqlx::query_scalar("INSERT INTO event_streams(owner_id,entity_kind,entity_id,next_seq) VALUES($1,'retention_test',$2,$3) RETURNING stream_id")
        .bind(owner).bind(&entity).bind(length+1).fetch_one(&mut *tx).await.expect("stream");
    sqlx::query("WITH inserted AS (
        INSERT INTO durable_events(stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id)
        SELECT $1,seq,'retention_test.seed',encode(sha256(convert_to($6,'UTF8')),'hex'),$2,clock_timestamp(),nextval(pg_get_serial_sequence('outbox','event_id'))
        FROM generate_series(1::bigint,$3::bigint) AS seq RETURNING event_id,seq,source_outbox_id)
        INSERT INTO outbox(event_id,domain_event_id,owner_id,entity_id,event_type,payload,delivered_at)
        SELECT source_outbox_id,event_id,$4,$2,'retention_test.seed',$6,CASE WHEN seq=$5 THEN NULL ELSE clock_timestamp() END FROM inserted")
        .bind(&stream).bind(&entity).bind(length).bind(owner).bind(pending).bind("development seed").execute(&mut *tx).await.expect("linked outbox rows");
    tx.commit().await.expect("seed commit");
    stream
}

#[tokio::test]
async fn batch_retention_is_bounded_contiguous_concurrent_and_preserves_legacy_atomicity() {
    let (store, events, owner, _) = fixture().await;
    let stream = retained_stream(&store, &owner, 260, 258).await;
    assert!(matches!(
        events.prune_delivered_batch(&stream, 260, 0).await,
        Err(EventError::Invalid)
    ));
    assert!(matches!(
        events.prune_delivered_batch(&stream, 260, 257).await,
        Err(EventError::Invalid)
    ));
    assert!(matches!(
        events.prune_delivered_batch(&stream, 261, 32).await,
        Err(EventError::Invalid)
    ));
    assert!(matches!(
        events.prune_delivered(&stream, 260).await,
        Err(EventError::Undelivered)
    ));
    let count = || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM durable_events WHERE stream_id=$1")
            .bind(&stream)
            .fetch_one(store.pool())
    };
    assert_eq!(count().await.expect("legacy rollback across chunks"), 260);
    let (left, right) = tokio::join!(
        events.prune_delivered_batch(&stream, 260, 100),
        events.prune_delivered_batch(&stream, 260, 100)
    );
    let (left, right) = (
        left.expect("first transaction"),
        right.expect("second transaction"),
    );
    assert_eq!((left.removed, right.removed), (100, 100));
    let mut cutoffs = [left.oldest_remaining, right.oldest_remaining];
    cutoffs.sort();
    assert_eq!(cutoffs, [101, 201]);
    let blocked = events
        .prune_delivered_batch(&stream, 260, 100)
        .await
        .expect("up to first pending");
    assert_eq!(
        (
            blocked.removed,
            blocked.oldest_remaining,
            blocked.blocked_at,
            blocked.complete
        ),
        (57, 258, Some(258), false)
    );
    assert_eq!(
        events
            .prune_delivered_batch(&stream, 260, 100)
            .await
            .expect("recheck")
            .removed,
        0
    );
    assert_eq!(count().await.expect("pending and later rows retained"), 3);
    sqlx::query("UPDATE outbox o SET delivered_at=clock_timestamp() FROM durable_events e WHERE e.stream_id=$1 AND e.seq=258 AND e.event_id=o.domain_event_id")
        .bind(&stream).execute(store.pool()).await.expect("acknowledgment");
    let final_batch = events
        .prune_delivered_batch(&stream, 260, 100)
        .await
        .expect("finish");
    assert_eq!(
        (
            final_batch.removed,
            final_batch.oldest_remaining,
            final_batch.complete
        ),
        (3, 261, true)
    );
    assert_eq!(
        events
            .prune_delivered_batch(&stream, 260, 100)
            .await
            .expect("repeat")
            .removed,
        0
    );
    assert!(matches!(
        events.read(&owner, &stream, "259", 8).await,
        Err(EventError::SnapshotRequired)
    ));
    assert!(
        events
            .read(&owner, &stream, "260", 8)
            .await
            .expect("head")
            .events
            .is_empty()
    );
}

#[tokio::test]
async fn retention_cannot_advance_over_a_missing_notification_or_change_ledger_postings() {
    let (store, events, owner, _) = fixture().await;
    let stream = retained_stream(&store, &owner, 3, 0).await;
    let id = Uuid::new_v4().to_string();
    let mut tx = store.pool().begin().await.expect("begin posting fixture");
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&id)
    .bind(format!("retention-fixture-{id}"))
    .bind(&owner)
    .execute(&mut *tx)
    .await
    .expect("transaction");
    sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,'test_debit',-17),($1,'test_credit',17)")
        .bind(&id).execute(&mut *tx).await.expect("balanced entries");
    tx.commit().await.expect("commit");
    sqlx::query("DELETE FROM outbox o USING durable_events e WHERE e.stream_id=$1 AND e.seq=2 AND e.event_id=o.domain_event_id")
        .bind(&stream).execute(store.pool()).await.expect("explicit corruption fixture");
    let err = events
        .prune_delivered_batch(&stream, 3, 3)
        .await
        .expect_err("no proof of delivery");
    assert!(
        matches!(err,EventError::Database(ref sql) if sql.as_database_error().and_then(|db|db.code()).as_deref()==Some("XX001"))
    );
    let pruned = events
        .prune_delivered_batch(&stream, 1, 1)
        .await
        .expect("unaffected prefix");
    assert_eq!((pruned.removed, pruned.oldest_remaining), (1, 2));
    let entries: Vec<i64> = sqlx::query_scalar(
        "SELECT amount FROM ledger_entries WHERE transaction_id=$1 ORDER BY amount",
    )
    .bind(&id)
    .fetch_all(store.pool())
    .await
    .expect("retained");
    assert_eq!(entries, [-17, 17]);
    assert_eq!(
        events
            .read(&owner, &stream, "1", 5)
            .await
            .expect("after safe prefix")
            .events
            .len(),
        2
    );
}
