use super::{DeviceSession, SessionError, WorkerSessionStore, sessions};
use orange_protocol::{
    grpc::worker::{IsolationObservation, IsolationObservationReceipt},
    isolation::Checked,
};
use sqlx::Row;

#[derive(Debug, thiserror::Error)]
pub(super) enum Failure {
    #[error("immutable observation conflict")]
    Conflict,
    #[error("reported capture is outside the acceptance window")]
    Expired,
    #[error("observation rate limit")]
    Limited,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}
impl WorkerSessionStore {
    pub(super) async fn record_isolation(
        &self,
        session: &DeviceSession,
        body: &IsolationObservation,
        checked: Checked,
    ) -> Result<IsolationObservationReceipt, Failure> {
        if i64::try_from(body.connection_epoch).ok() != Some(session.connection_epoch) {
            return Err(SessionError::Stale.into());
        }
        let mut tx = self.pool.begin().await?;
        // authorize_in locks the node and session before quota or receipt lookup.
        self.authorize_in(&mut tx, session).await?;
        let stored=sqlx::query("SELECT observation_id::text,observation_digest,disposition FROM worker_isolation_observations
            WHERE node_id=$1 AND session_id=$2 AND connection_epoch=$3 AND request_id=$4")
            .bind(&session.node_id).bind(&session.session_id).bind(session.connection_epoch).bind(&body.request_id)
            .fetch_optional(&mut *tx).await?;
        let replay = stored.is_some();
        let id = if let Some(row) = stored {
            if row.try_get::<&str, _>("observation_digest")? != checked.digest
                || row.try_get::<&str, _>("disposition")? != checked.disposition
            {
                return Err(Failure::Conflict);
            }
            row.try_get::<String, _>("observation_id")?
        } else {
            let recent:i64=sqlx::query_scalar("SELECT count(*) FROM worker_isolation_observations WHERE node_id=$1 AND received_at>clock_timestamp()-interval '1 hour'")
                .bind(&session.node_id).fetch_one(&mut *tx).await?;
            if recent >= 24 {
                return Err(Failure::Limited);
            }
            sqlx::query_scalar("INSERT INTO worker_isolation_observations(node_id,session_id,connection_epoch,request_id,certificate_sha256,
                node_policy_generation,capabilities_digest,profile_id,policy_digest,probe_digest,observation_digest,process_observation,disposition,reported_capture_unix)
                SELECT node_id,session_id,connection_epoch,$4,certificate_sha256,n.policy_generation,capabilities_digest,$5,$6,$7,$8,$9,$10,$11
                FROM worker_sessions JOIN nodes n USING(node_id)
                WHERE node_id=$1 AND session_id=$2 AND connection_epoch=$3 AND n.policy_generation=$12 RETURNING observation_id::text")
                .bind(&session.node_id).bind(&session.session_id).bind(session.connection_epoch).bind(&body.request_id).bind(&body.profile_id)
                .bind(&body.oci_policy_sha256).bind(&body.linux_probe_sha256).bind(&checked.digest).bind(checked.data).bind(checked.disposition).bind(body.reported_capture_unix).bind(checked.generation)
                .fetch_optional(&mut *tx).await?.ok_or(SessionError::Stale)?
        };
        if !replay {
            let fresh:bool=sqlx::query_scalar("SELECT $1::bigint BETWEEN n-300 AND n+30 FROM (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint AS n) t")
                .bind(body.reported_capture_unix).fetch_one(&mut *tx).await?;
            if !fresh {
                return Err(Failure::Expired);
            }
        }
        sessions::finish(tx, session).await?;
        Ok(IsolationObservationReceipt {
            report_version: 1,
            session_id: session.session_id.clone(),
            connection_epoch: body.connection_epoch,
            request_id: body.request_id.clone(),
            observation_id: id,
            observation_digest: checked.digest,
            disposition: checked.disposition.into(),
            supply_authorized: false,
            wasm_enabled: false,
        })
    }
}

/// Diagnostic evidence has no downstream spending or execution rights.
pub async fn expire(pool: &sqlx::PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("WITH due AS (SELECT observation_id FROM worker_isolation_observations WHERE received_at<=clock_timestamp()-interval '30 days'
        ORDER BY received_at,observation_id LIMIT 128 FOR UPDATE SKIP LOCKED)
        DELETE FROM worker_isolation_observations e USING due WHERE e.observation_id=due.observation_id")
        .execute(pool).await?.rows_affected())
}
