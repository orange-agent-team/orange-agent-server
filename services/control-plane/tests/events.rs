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
