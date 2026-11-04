//! PostgreSQL notification delivery. Inbox completion and database effects share one transaction.
//! Never use this retry protocol to repeat non-transactional external tool/payment effects.
use orange_protocol::{events::DurableEvent, request_digest, valid_id};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("invalid notification or consumer")]
    Invalid,
    #[error("message identity or immutable payload conflict")]
    Conflict,
    #[error("delivery lease expired, replaced, completed or revoked")]
    StaleLease,
    #[error("queue database operation failed")]
    Database(#[from] sqlx::Error),
}
#[derive(Clone)]
pub struct EventQueue {
    pool: PgPool,
}
#[derive(Debug, Clone)]
pub struct DeliveryLease {
    pub consumer_id: String,
    pub event_id: String,
    pub lease_id: String,
    pub attempt: i32,
}
pub struct DeliveryTransaction {
    transaction: Transaction<'static, Postgres>,
    lease: DeliveryLease,
    event: DurableEvent,
    payload: String,
}
impl DeliveryTransaction {
    pub fn event(&self) -> &DurableEvent {
        &self.event
    }
    pub fn payload(&self) -> &str {
        &self.payload
    }
    /// SQL domain mutations must use this transaction. Dropping it rolls back both effects and ACK.
    pub fn transaction(&mut self) -> &mut Transaction<'static, Postgres> {
        &mut self.transaction
    }
    pub async fn rollback(self) -> Result<(), QueueError> {
        self.transaction.rollback().await?;
        Ok(())
    }
    pub async fn finish(mut self) -> Result<(), QueueError> {
        let count=sqlx::query("UPDATE inbox SET state='done',completed_at=clock_timestamp(),lease_id=NULL,lease_expires_at=NULL,lease_subscription_version=NULL WHERE consumer_id=$1 AND event_id=$2 AND state='leased' AND lease_id=$3 AND lease_expires_at>clock_timestamp()")
            .bind(&self.lease.consumer_id).bind(&self.lease.event_id).bind(&self.lease.lease_id).execute(&mut *self.transaction).await?.rows_affected();
        if count != 1 {
            return Err(QueueError::StaleLease);
        }
        self.transaction.commit().await?;
        Ok(())
    }
}
impl EventQueue {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    /// Trusted service registration only, not a user-facing subscription or authorization API.
    pub async fn subscribe(
        &self,
        consumer: &str,
        owner: &str,
        event_type: &str,
    ) -> Result<(), QueueError> {
        validate(&[consumer, owner, event_type])?;
        sqlx::query("INSERT INTO queue_subscriptions(consumer_id,owner_id,event_type) VALUES($1,$2,$3) ON CONFLICT(consumer_id,owner_id,event_type) DO UPDATE SET active=TRUE,version=CASE WHEN queue_subscriptions.active THEN queue_subscriptions.version ELSE queue_subscriptions.version+1 END")
            .bind(consumer).bind(owner).bind(event_type).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn unsubscribe(
        &self,
        consumer: &str,
        owner: &str,
        event_type: &str,
    ) -> Result<(), QueueError> {
        validate(&[consumer, owner, event_type])?;
        sqlx::query("UPDATE queue_subscriptions SET active=FALSE,version=version+1 WHERE consumer_id=$1 AND owner_id=$2 AND event_type=$3 AND active")
            .bind(consumer).bind(owner).bind(event_type).execute(&self.pool).await?;
        Ok(())
    }
    /// Copies to every currently registered consumer atomically; acceptance is not business completion.
    pub async fn publish_owner(&self, owner: &str, limit: u16) -> Result<usize, QueueError> {
        validate(&[owner])?;
        if !(1..=200).contains(&limit) {
            return Err(QueueError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query("SELECT o.*,e.stream_id,e.seq,e.schema_version,e.payload_digest,e.correlation_id,e.causation_id,FLOOR(EXTRACT(EPOCH FROM e.occurred_at))::bigint AS timestamp,s.entity_kind FROM outbox o JOIN durable_events e ON e.event_id=o.domain_event_id JOIN event_streams s ON s.stream_id=e.stream_id WHERE o.owner_id=$1 AND o.delivered_at IS NULL AND EXISTS(SELECT 1 FROM queue_subscriptions q WHERE q.owner_id=o.owner_id AND q.event_type=o.event_type AND q.active) ORDER BY o.event_id LIMIT $2 FOR UPDATE OF o SKIP LOCKED")
            .bind(owner).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let mut published = 0;
        for row in rows {
            let payload: String = row.try_get("payload")?;
            let expected: String = row.try_get("payload_digest")?;
            if hex::encode(Sha256::digest(payload.as_bytes())) != expected {
                return Err(QueueError::Conflict);
            }
            let event = DurableEvent {
                schema_version: u16::try_from(row.try_get::<i16, _>("schema_version")?)
                    .map_err(|_| QueueError::Invalid)?,
                event_id: row.try_get("domain_event_id")?,
                stream_id: row.try_get("stream_id")?,
                seq: row.try_get::<i64, _>("seq")?.to_string(),
                owner_id: owner.into(),
                entity_kind: row.try_get("entity_kind")?,
                entity_id: row.try_get("entity_id")?,
                event_type: row.try_get("event_type")?,
                occurred_at: OffsetDateTime::from_unix_timestamp(row.try_get("timestamp")?)
                    .map_err(|_| QueueError::Invalid)?
                    .format(&Rfc3339)
                    .map_err(|_| QueueError::Invalid)?,
                payload_digest: expected,
                correlation_id: row.try_get("correlation_id")?,
                causation_id: row.try_get("causation_id")?,
            };
            let envelope = serde_json::to_string(&event).map_err(|_| QueueError::Invalid)?;
            let digest =
                request_digest("inbox-envelope-v1", &event).map_err(|_| QueueError::Invalid)?;
            let subscriptions=sqlx::query("SELECT consumer_id FROM queue_subscriptions WHERE owner_id=$1 AND event_type=$2 AND active FOR SHARE")
                .bind(owner).bind(&event.event_type).fetch_all(&mut *tx).await?;
            if subscriptions.is_empty() {
                continue;
            }
            for subscription in subscriptions {
                let consumer: String = subscription.try_get("consumer_id")?;
                sqlx::query("INSERT INTO inbox(consumer_id,event_id,owner_id,event_type,envelope,payload,envelope_digest) VALUES($1,$2,$3,$4,$5::jsonb,$6,$7) ON CONFLICT(consumer_id,event_id) DO NOTHING")
                    .bind(&consumer).bind(&event.event_id).bind(owner).bind(&event.event_type).bind(&envelope).bind(&payload).bind(&digest).execute(&mut *tx).await?;
                let stored: String = sqlx::query_scalar(
                    "SELECT envelope_digest FROM inbox WHERE consumer_id=$1 AND event_id=$2",
                )
                .bind(&consumer)
                .bind(&event.event_id)
                .fetch_one(&mut *tx)
                .await?;
                if stored != digest {
                    return Err(QueueError::Conflict);
                }
            }
            sqlx::query("UPDATE outbox SET delivered_at=clock_timestamp() WHERE event_id=$1")
                .bind(row.try_get::<i64, _>("event_id")?)
                .execute(&mut *tx)
                .await?;
            published += 1;
        }
        tx.commit().await?;
        Ok(published)
    }
    pub async fn claim(&self, consumer: &str) -> Result<Option<DeliveryLease>, QueueError> {
        validate(&[consumer])?;
        let mut tx = self.pool.begin().await?;
        // Expired final leases are visible dead letters, never silently discarded or retried forever.
        sqlx::query("WITH candidates AS (SELECT consumer_id,event_id FROM inbox WHERE consumer_id=$1 AND state='leased' AND lease_expires_at<=clock_timestamp() AND attempts>=3 FOR UPDATE SKIP LOCKED), exhausted AS (UPDATE inbox i SET state='dead',last_error='LEASE_EXHAUSTED',lease_id=NULL,lease_expires_at=NULL,lease_subscription_version=NULL FROM candidates c WHERE i.consumer_id=c.consumer_id AND i.event_id=c.event_id RETURNING i.consumer_id,i.event_id,i.attempts) INSERT INTO queue_dead_letters(consumer_id,event_id,reason,attempts) SELECT consumer_id,event_id,'LEASE_EXHAUSTED',attempts FROM exhausted ON CONFLICT DO NOTHING").bind(consumer).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT i.event_id,i.attempts,s.version FROM inbox i JOIN queue_subscriptions s ON s.consumer_id=i.consumer_id AND s.owner_id=i.owner_id AND s.event_type=i.event_type WHERE i.consumer_id=$1 AND s.active AND i.attempts<3 AND ((i.state='ready' AND i.available_at<=clock_timestamp()) OR (i.state='leased' AND i.lease_expires_at<=clock_timestamp())) ORDER BY i.available_at,i.event_id LIMIT 1 FOR UPDATE OF i SKIP LOCKED FOR SHARE OF s")
            .bind(consumer).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let event_id: String = row.try_get("event_id")?;
        let lease_id = Uuid::new_v4().to_string();
        let attempt = row.try_get::<i32, _>("attempts")? + 1;
        sqlx::query("UPDATE inbox SET state='leased',attempts=$3,lease_id=$4,lease_expires_at=clock_timestamp()+interval '30 seconds',lease_subscription_version=$5 WHERE consumer_id=$1 AND event_id=$2")
            .bind(consumer).bind(&event_id).bind(attempt).bind(&lease_id).bind(row.try_get::<i64,_>("version")?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(DeliveryLease {
            consumer_id: consumer.into(),
            event_id,
            lease_id,
            attempt,
        }))
    }
    pub async fn begin_delivery(
        &self,
        lease: &DeliveryLease,
    ) -> Result<DeliveryTransaction, QueueError> {
        let mut tx = self.pool.begin().await?;
        let row = lock_lease(&mut tx, lease).await?;
        let event: DurableEvent = serde_json::from_str(&row.try_get::<String, _>("envelope_text")?)
            .map_err(|_| QueueError::Invalid)?;
        let payload: String = row.try_get("payload")?;
        if event.schema_version != 1
            || event.event_id != lease.event_id
            || event.owner_id != row.try_get::<String, _>("owner_id")?
            || event.payload_digest != hex::encode(Sha256::digest(payload.as_bytes()))
            || request_digest("inbox-envelope-v1", &event).map_err(|_| QueueError::Invalid)?
                != row.try_get::<String, _>("envelope_digest")?
        {
            return Err(QueueError::Conflict);
        }
        Ok(DeliveryTransaction {
            transaction: tx,
            lease: lease.clone(),
            event,
            payload,
        })
    }
    pub async fn fail(
        &self,
        lease: &DeliveryLease,
        code: &str,
        permanent: bool,
    ) -> Result<(), QueueError> {
        validate(&[code])?;
        let mut tx = self.pool.begin().await?;
        let row = lock_lease(&mut tx, lease).await?;
        let attempts: i32 = row.try_get("attempts")?;
        let dead = permanent || attempts >= 3;
        sqlx::query("UPDATE inbox SET state=$4,last_error=$5,available_at=clock_timestamp()+make_interval(secs=>$6),lease_id=NULL,lease_expires_at=NULL,lease_subscription_version=NULL WHERE consumer_id=$1 AND event_id=$2 AND lease_id=$3")
            .bind(&lease.consumer_id).bind(&lease.event_id).bind(&lease.lease_id).bind(if dead{"dead"}else{"ready"}).bind(code).bind(f64::from(1_u32<<attempts)).execute(&mut *tx).await?;
        if dead {
            sqlx::query("INSERT INTO queue_dead_letters(consumer_id,event_id,reason,attempts) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(&lease.consumer_id).bind(&lease.event_id).bind(code).bind(attempts).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
fn validate(ids: &[&str]) -> Result<(), QueueError> {
    if ids.iter().all(|s| valid_id(s)) {
        Ok(())
    } else {
        Err(QueueError::Invalid)
    }
}
async fn lock_lease(
    tx: &mut Transaction<'_, Postgres>,
    lease: &DeliveryLease,
) -> Result<sqlx::postgres::PgRow, QueueError> {
    validate(&[&lease.consumer_id, &lease.event_id, &lease.lease_id])?;
    let row=sqlx::query("SELECT i.*,i.envelope::text AS envelope_text FROM inbox i JOIN queue_subscriptions s ON s.consumer_id=i.consumer_id AND s.owner_id=i.owner_id AND s.event_type=i.event_type WHERE i.consumer_id=$1 AND i.event_id=$2 AND i.lease_id=$3 AND i.state='leased' AND i.lease_expires_at>clock_timestamp() AND s.active AND i.lease_subscription_version=s.version FOR UPDATE OF i FOR SHARE OF s")
        .bind(&lease.consumer_id).bind(&lease.event_id).bind(&lease.lease_id).fetch_optional(&mut **tx).await?.ok_or(QueueError::StaleLease)?;
    Ok(row)
}
