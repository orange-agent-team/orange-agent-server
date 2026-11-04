use orange_control_plane::{
    events::EventStore,
    queue::{DeliveryLease, EventQueue, QueueError},
    store::Store,
};
use orange_domain::Credits;
use orange_protocol::{CreateBudget, Money};
use uuid::Uuid;

async fn fixture() -> (Store, EventQueue, String, String) {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("actual PostgreSQL required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let owner = Uuid::new_v4().to_string();
    let consumer = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("owner fixture");
    let queue = EventQueue::new(store.pool().clone());
    (store, queue, owner, consumer)
}
async fn event(store: &Store, owner: &str, key: &str) -> String {
    store
        .create_budget(
            owner,
            key,
            &CreateBudget {
                scope_id: key.into(),
                limit: Money(Credits::new(5).expect("limit")),
            },
        )
        .await
        .expect("real domain event")
        .root_budget_id
}
async fn lease(queue: &EventQueue, consumer: &str) -> DeliveryLease {
    queue
        .claim(consumer)
        .await
        .expect("claim")
        .expect("available delivery")
}
async fn expire(store: &Store, lease: &DeliveryLease) {
    sqlx::query("UPDATE inbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE consumer_id=$1 AND event_id=$2").bind(&lease.consumer_id).bind(&lease.event_id).execute(store.pool()).await.expect("controlled expiration fixture");
}

#[tokio::test]
async fn concurrent_publish_fans_out_once_and_acceptance_is_not_completion() {
    let (store, queue, owner, consumer) = fixture().await;
    let second = Uuid::new_v4().to_string();
    event(&store, &owner, "one").await;
    assert_eq!(
        queue
            .publish_owner(&owner, 50)
            .await
            .expect("no subscribers"),
        0
    );
    queue
        .subscribe(&consumer, &owner, "budget.created")
        .await
        .expect("subscribe");
    queue
        .subscribe(&second, &owner, "budget.created")
        .await
        .expect("second subscriber");
    let (a, b) = tokio::join!(
        queue.publish_owner(&owner, 50),
        queue.publish_owner(&owner, 50)
    );
    assert_eq!(a.expect("publish") + b.expect("competing publish"), 1);
    assert_eq!(queue.publish_owner(&owner, 50).await.expect("repeat"), 0);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inbox WHERE owner_id=$1 AND state='ready'")
            .bind(&owner)
            .fetch_one(store.pool())
            .await
            .expect("accepted");
    assert_eq!(count, 2);
    assert!(
        queue
            .claim("unregistered-consumer")
            .await
            .expect("isolated")
            .is_none()
    );
    let (a, b) = tokio::join!(queue.claim(&consumer), queue.claim(&consumer));
    assert_ne!(
        a.expect("claim").is_some(),
        b.expect("competing claim").is_some()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inbox WHERE owner_id=$1 AND state='done'")
            .bind(&owner)
            .fetch_one(store.pool())
            .await
            .expect("not completed");
    assert_eq!(count, 0);
}
#[tokio::test]
async fn database_effect_and_completion_commit_together_and_stale_lease_never_applies() {
    let (store, queue, owner, consumer) = fixture().await;
    queue
        .subscribe(&consumer, &owner, "budget.created")
        .await
        .expect("subscribe");
    let root = event(&store, &owner, "one").await;
    queue.publish_owner(&owner, 50).await.expect("publish");
    let first = lease(&queue, &consumer).await;
    let mut delivery = queue.begin_delivery(&first).await.expect("begin");
    sqlx::query("UPDATE budget_roots SET limit_amount=7 WHERE root_budget_id=$1")
        .bind(&root)
        .execute(&mut **delivery.transaction())
        .await
        .expect("transactional domain fixture effect");
    delivery.rollback().await.expect("handler aborted");
    assert_eq!(
        store
            .budget(&owner, &root)
            .await
            .expect("rolled back")
            .limit
            .0
            .units(),
        5
    );
    expire(&store, &first).await;
    let second = lease(&queue, &consumer).await;
    assert_eq!(second.attempt, 2);
    assert_ne!(first.lease_id, second.lease_id);
    assert!(matches!(
        queue.begin_delivery(&first).await,
        Err(QueueError::StaleLease)
    ));
    let mut delivery = queue.begin_delivery(&second).await.expect("new lease");
    assert_eq!(delivery.event().owner_id, owner);
    assert!(!delivery.payload().is_empty());
    sqlx::query("UPDATE budget_roots SET limit_amount=7 WHERE root_budget_id=$1")
        .bind(&root)
        .execute(&mut **delivery.transaction())
        .await
        .expect("effect");
    delivery.finish().await.expect("effect and receipt commit");
    assert_eq!(
        store
            .budget(&owner, &root)
            .await
            .expect("committed")
            .limit
            .0
            .units(),
        7
    );
    assert!(
        queue
            .claim(&consumer)
            .await
            .expect("not redelivered")
            .is_none()
    );
    assert!(matches!(
        queue.begin_delivery(&second).await,
        Err(QueueError::StaleLease)
    ));
    assert!(
        sqlx::query("UPDATE inbox SET payload='tampered' WHERE consumer_id=$1")
            .bind(&consumer)
            .execute(store.pool())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn retry_exhaustion_is_durable_and_revoked_subscription_cannot_resurrect_old_lease() {
    let (store, queue, owner, consumer) = fixture().await;
    queue
        .subscribe(&consumer, &owner, "budget.created")
        .await
        .expect("subscribe");
    event(&store, &owner, "one").await;
    queue.publish_owner(&owner, 50).await.expect("publish");
    let old = lease(&queue, &consumer).await;
    queue
        .unsubscribe(&consumer, &owner, "budget.created")
        .await
        .expect("revoke");
    assert!(matches!(
        queue.begin_delivery(&old).await,
        Err(QueueError::StaleLease)
    ));
    queue
        .subscribe(&consumer, &owner, "budget.created")
        .await
        .expect("new subscription generation");
    assert!(matches!(
        queue.begin_delivery(&old).await,
        Err(QueueError::StaleLease)
    ));
    expire(&store, &old).await;
    let second = lease(&queue, &consumer).await;
    queue
        .fail(&second, "TEMPORARY_FIXTURE", false)
        .await
        .expect("bounded retry");
    assert!(queue.claim(&consumer).await.expect("backoff").is_none());
    sqlx::query(
        "UPDATE inbox SET available_at=clock_timestamp()-interval '1 second' WHERE consumer_id=$1",
    )
    .bind(&consumer)
    .execute(store.pool())
    .await
    .expect("advance controlled retry clock");
    let third = lease(&queue, &consumer).await;
    assert_eq!(third.attempt, 3);
    queue
        .fail(&third, "TEMPORARY_FIXTURE", false)
        .await
        .expect("dead letter");
    assert!(
        queue
            .claim(&consumer)
            .await
            .expect("no fourth attempt")
            .is_none()
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM queue_dead_letters WHERE consumer_id=$1 AND attempts=3",
    )
    .bind(&consumer)
    .fetch_one(store.pool())
    .await
    .expect("durable dead letter");
    assert_eq!(count, 1);
}
#[tokio::test]
async fn delivered_event_retention_preserves_pending_inbox_and_last_expired_attempt_is_recorded() {
    let (store, queue, owner, consumer) = fixture().await;
    queue
        .subscribe(&consumer, &owner, "budget.created")
        .await
        .expect("subscribe");
    let root = event(&store, &owner, "one").await;
    queue.publish_owner(&owner, 50).await.expect("publish");
    let events = EventStore::new(store.pool().clone());
    let stream = events
        .discover(&owner, "budget", &root)
        .await
        .expect("stream");
    events
        .prune_delivered(&stream.stream_id, 1)
        .await
        .expect("notification retention after durable acceptance");
    for attempt in 1..=3 {
        let delivery = lease(&queue, &consumer).await;
        assert_eq!(delivery.attempt, attempt);
        expire(&store, &delivery).await;
    }
    assert!(
        queue
            .claim(&consumer)
            .await
            .expect("exhaust expired leases")
            .is_none()
    );
    let reason: String =
        sqlx::query_scalar("SELECT reason FROM queue_dead_letters WHERE consumer_id=$1")
            .bind(&consumer)
            .fetch_one(store.pool())
            .await
            .expect("not lost after source retention");
    assert_eq!(reason, "LEASE_EXHAUSTED");
}
