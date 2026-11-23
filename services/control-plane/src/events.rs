use orange_protocol::{events::*, valid_id};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("invalid event query")]
    Invalid,
    #[error("owner is not active")]
    Forbidden,
    #[error("event stream not found")]
    NotFound,
    #[error("event cursor expired; obtain current entity snapshot")]
    SnapshotRequired,
    #[error("undelivered events cannot be pruned")]
    Undelivered,
    #[error("event database operation failed")]
    Database(#[from] sqlx::Error),
}
#[derive(Clone)]
pub struct EventStore {
    pool: PgPool,
}
impl EventStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn discover(
        &self,
        owner: &str,
        kind: &str,
        entity: &str,
    ) -> Result<EventStreamView, EventError> {
        if ![owner, kind, entity].iter().all(|s| valid_id(s)) {
            return Err(EventError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        owner_active(&mut tx, owner).await?;
        let row=sqlx::query("SELECT * FROM event_streams WHERE owner_id=$1 AND entity_kind=$2 AND entity_id=$3 FOR SHARE")
            .bind(owner).bind(kind).bind(entity).fetch_optional(&mut *tx).await?.ok_or(EventError::NotFound)?;
        let view = stream_view(&row)?;
        tx.commit().await?;
        Ok(view)
    }
    pub async fn read(
        &self,
        owner: &str,
        stream: &str,
        after: &str,
        limit: u16,
    ) -> Result<EventPage, EventError> {
        if !valid_id(owner) || !valid_id(stream) || !(1..=200).contains(&limit) {
            return Err(EventError::Invalid);
        }
        let after = parse_cursor(after).ok_or(EventError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        owner_active(&mut tx, owner).await?;
        let row =
            sqlx::query("SELECT * FROM event_streams WHERE stream_id=$1 AND owner_id=$2 FOR SHARE")
                .bind(stream)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(EventError::NotFound)?;
        let oldest: i64 = row.try_get("oldest_seq")?;
        let next: i64 = row.try_get("next_seq")?;
        if after < oldest - 1 {
            return Err(EventError::SnapshotRequired);
        }
        if after >= next {
            return Err(EventError::Invalid);
        }
        let rows=sqlx::query("SELECT *,FLOOR(EXTRACT(EPOCH FROM occurred_at))::bigint AS timestamp FROM durable_events WHERE stream_id=$1 AND seq>$2 ORDER BY seq LIMIT $3").bind(stream).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let mut events = Vec::new();
        let mut last = after;
        for event in rows {
            let sequence: i64 = event.try_get("seq")?;
            if sequence != last + 1 {
                return Err(EventError::SnapshotRequired);
            }
            last = sequence;
            let stamp = OffsetDateTime::from_unix_timestamp(event.try_get("timestamp")?)
                .map_err(|_| EventError::Invalid)?
                .format(&Rfc3339)
                .map_err(|_| EventError::Invalid)?;
            events.push(DurableEvent {
                schema_version: u16::try_from(event.try_get::<i16, _>("schema_version")?)
                    .map_err(|_| EventError::Invalid)?,
                event_id: event.try_get("event_id")?,
                stream_id: stream.into(),
                seq: sequence.to_string(),
                owner_id: owner.into(),
                entity_kind: row.try_get("entity_kind")?,
                entity_id: row.try_get("entity_id")?,
                event_type: event.try_get("event_type")?,
                occurred_at: stamp,
                payload_digest: event.try_get("payload_digest")?,
                correlation_id: event.try_get("correlation_id")?,
                causation_id: event.try_get("causation_id")?,
            });
        }
        tx.commit().await?;
        Ok(EventPage {
            stream_id: stream.into(),
            events,
            next_cursor: last.to_string(),
        })
    }
    /// Trusted retention maintenance only; no public endpoint. Money/idempotency history is untouched.
    pub async fn prune_delivered(&self, stream: &str, through: i64) -> Result<(), EventError> {
        if !valid_id(stream) || through < 0 {
            return Err(EventError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT next_seq,oldest_seq FROM event_streams WHERE stream_id=$1 FOR UPDATE",
        )
        .bind(stream)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(EventError::NotFound)?;
        if through >= row.try_get::<i64, _>("next_seq")? {
            return Err(EventError::Invalid);
        }
        if through < row.try_get::<i64, _>("oldest_seq")? {
            tx.commit().await?;
            return Ok(());
        }
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM outbox o JOIN durable_events e ON e.event_id=o.domain_event_id WHERE e.stream_id=$1 AND e.seq<=$2 AND o.delivered_at IS NULL)").bind(stream).bind(through).fetch_one(&mut *tx).await?;
        if pending {
            return Err(EventError::Undelivered);
        }
        sqlx::query("DELETE FROM outbox o USING durable_events e WHERE o.domain_event_id=e.event_id AND e.stream_id=$1 AND e.seq<=$2").bind(stream).bind(through).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM durable_events WHERE stream_id=$1 AND seq<=$2")
            .bind(stream)
            .bind(through)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE event_streams SET oldest_seq=$2+1 WHERE stream_id=$1")
            .bind(stream)
            .bind(through)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
fn stream_view(row: &sqlx::postgres::PgRow) -> Result<EventStreamView, sqlx::Error> {
    Ok(EventStreamView {
        stream_id: row.try_get("stream_id")?,
        entity_kind: row.try_get("entity_kind")?,
        entity_id: row.try_get("entity_id")?,
        next_seq: row.try_get::<i64, _>("next_seq")?.to_string(),
        oldest_seq: row.try_get::<i64, _>("oldest_seq")?.to_string(),
    })
}
async fn owner_active(tx: &mut Transaction<'_, Postgres>, owner: &str) -> Result<(), EventError> {
    if sqlx::query("SELECT owner_id FROM owners WHERE owner_id=$1 AND state='active' FOR SHARE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .is_none()
    {
        return Err(EventError::Forbidden);
    }
    Ok(())
}
/// Same transaction as the domain mutation; the stream row makes per-entity committed order explicit.
pub(crate) async fn append(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    entity: &str,
    kind: &str,
    payload: &str,
) -> Result<(), sqlx::Error> {
    append_core(tx, owner, entity, kind, payload, None).await
}

pub(crate) async fn append_attempt(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    job: &str,
    kind: &str,
    payload: &str,
    attempt: &str,
) -> Result<(), sqlx::Error> {
    append_core(tx, owner, job, kind, payload, Some(attempt)).await
}

async fn append_core(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    entity: &str,
    kind: &str,
    payload: &str,
    attempt: Option<&str>,
) -> Result<(), sqlx::Error> {
    let digest = hex::encode(Sha256::digest(payload.as_bytes()));
    let entity_kind = kind
        .split('.')
        .next()
        .ok_or_else(|| sqlx::Error::Protocol("missing event kind".into()))?;
    let row=sqlx::query("INSERT INTO event_streams(owner_id,entity_kind,entity_id,next_seq) VALUES($1,$2,$3,2) ON CONFLICT(owner_id,entity_kind,entity_id) DO UPDATE SET next_seq=event_streams.next_seq+1 RETURNING stream_id,next_seq-1 AS seq")
        .bind(owner).bind(entity_kind).bind(entity).fetch_one(&mut **tx).await?;
    let stream: String = row.try_get("stream_id")?;
    let seq: i64 = row.try_get("seq")?;
    let outbox: i64 =
        sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('outbox','event_id'))")
            .fetch_one(&mut **tx)
            .await?;
    // Events without attempt provenance retain their original insertion contract.
    // Only an actual Worker attempt requires the schema-68 relation and its ownership check.
    let statement = if attempt.is_some() {
        "INSERT INTO durable_events(stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id,attempt_id) VALUES($1,$2,$3,$4,$5,clock_timestamp(),$6,$7) RETURNING event_id"
    } else {
        "INSERT INTO durable_events(stream_id,seq,event_type,payload_digest,correlation_id,occurred_at,source_outbox_id) VALUES($1,$2,$3,$4,$5,clock_timestamp(),$6) RETURNING event_id"
    };
    let mut insert = sqlx::query_scalar(statement)
        .bind(stream)
        .bind(seq)
        .bind(kind)
        .bind(&digest)
        .bind(entity)
        .bind(outbox);
    if let Some(id) = attempt {
        insert = insert.bind(id);
    }
    let event: String = insert.fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO outbox(event_id,owner_id,entity_id,event_type,payload,domain_event_id) VALUES($1,$2,$3,$4,$5,$6)").bind(outbox).bind(owner).bind(entity).bind(kind).bind(payload).bind(event).execute(&mut **tx).await?;
    Ok(())
}
