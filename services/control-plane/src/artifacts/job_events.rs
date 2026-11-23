use super::*;
use orange_protocol::{
    events::parse_cursor,
    job_events::{JobEvent, JobEventCursor, JobHistory},
};
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct HistoryQuery {
    pub stream_id: Option<String>,
    pub after: Option<String>,
    pub limit: Option<u16>,
}

impl ArtifactStore {
    pub(super) async fn job_history(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        job: &str,
        query: &HistoryQuery,
    ) -> Result<JobHistory, ArtifactError> {
        if !valid_id(owner) || !valid_id(job) {
            return Err(ArtifactError::Invalid);
        }
        let limit = query.limit.unwrap_or(32);
        if !(1..=64).contains(&limit) {
            return Err(ArtifactError::Invalid);
        }
        let requested = match (&query.stream_id, &query.after) {
            (Some(id), Some(seq)) if valid_id(id) => {
                Some((id, parse_cursor(seq).ok_or(ArtifactError::Invalid)?))
            }
            (None, None) => None,
            _ => return Err(ArtifactError::Invalid),
        };
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let status=sqlx::query("SELECT * FROM jobs WHERE job_id=$1 AND owner_id=$2 AND submitting_user_id=$3 FOR SHARE")
            .bind(job).bind(owner).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        let stream:Option<(String,i64,i64)>=sqlx::query_as("SELECT stream_id,oldest_seq,next_seq FROM event_streams WHERE owner_id=$1 AND entity_kind='job' AND entity_id=$2 FOR SHARE")
            .bind(owner).bind(job).fetch_optional(&mut *tx).await?;
        let history = match (requested, stream.as_ref()) {
            (Some((id, after)), Some((stream_id, oldest, next)))
                if id == stream_id && after >= oldest - 1 =>
            {
                if after >= *next {
                    return Err(ArtifactError::Invalid);
                }
                let rows=sqlx::query("SELECT event_id,seq,event_type,attempt_id,payload_digest,FLOOR(EXTRACT(EPOCH FROM occurred_at))::bigint AS occurred FROM durable_events WHERE stream_id=$1 AND seq>$2 ORDER BY seq LIMIT $3")
                    .bind(id).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
                let available = next - 1 - after;
                if rows.len() as i64 != available.min(i64::from(limit)) {
                    return Err(ArtifactError::Integrity);
                }
                let mut events = Vec::with_capacity(rows.len());
                let mut last = after;
                for row in rows {
                    let sequence: i64 = row.try_get("seq")?;
                    if sequence != last + 1 {
                        return Err(ArtifactError::Integrity);
                    }
                    last = sequence;
                    let occurred_at = OffsetDateTime::from_unix_timestamp(row.try_get("occurred")?)
                        .map_err(|_| ArtifactError::Integrity)?
                        .format(&Rfc3339)
                        .map_err(|_| ArtifactError::Integrity)?;
                    events.push(JobEvent {
                        event_id: row.try_get("event_id")?,
                        seq: last.to_string(),
                        event_type: row.try_get("event_type")?,
                        attempt_id: row.try_get("attempt_id")?,
                        occurred_at,
                        payload_digest: row.try_get("payload_digest")?,
                    });
                }
                JobHistory::Page {
                    job_id: job.to_owned(),
                    cursor: JobEventCursor {
                        stream_id: id.clone(),
                        after: last.to_string(),
                    },
                    events,
                }
            }
            _ => {
                let view = crate::store::job_view(&status).map_err(|_| ArtifactError::Integrity)?;
                let revision = status.try_get::<i64, _>("state_revision")?.to_string();
                let cursor = stream.map(|(stream_id, _, next)| JobEventCursor {
                    stream_id,
                    after: (next - 1).to_string(),
                });
                if requested.is_none() {
                    JobHistory::Snapshot {
                        job: view,
                        job_version: revision,
                        cursor,
                    }
                } else {
                    JobHistory::Reset {
                        job: view,
                        job_version: revision,
                        cursor,
                    }
                }
            }
        };
        authority::finish(tx, actor).await?;
        Ok(history)
    }
}
