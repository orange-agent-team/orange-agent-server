//! Durable negative ACK receipts. Historical lookup never changes execution,
//! releases a resource, retries a Job or moves money.
use super::{DeviceSession, SessionError, WorkerSessionStore, sessions::finish};
use orange_protocol::{execution::acknowledge_digest, grpc::worker};
use sqlx::{Postgres, Row, Transaction};
pub(super) async fn receipt_in(
    tx: &mut Transaction<'_, Postgres>,
    s: &DeviceSession,
    ack: &worker::Acknowledge,
) -> Result<worker::RejectionReceipt, SessionError> {
    if !matches!(
        worker::AckResult::try_from(ack.result),
        Ok(worker::AckResult::ResourceRejected | worker::AckResult::ProtocolRejected)
    ) {
        return Err(SessionError::Invalid);
    }
    let row = sqlx::query(
        "SELECT * FROM worker_execution_attempts WHERE node_id=$1 AND command_id=$2 FOR UPDATE",
    )
    .bind(&s.node_id)
    .bind(&ack.command_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(SessionError::Stale)?;
    let a = super::execution::decode(&row)?;
    let digest = acknowledge_digest(ack, &a).ok_or(SessionError::Invalid)?;
    if row.try_get::<String, _>("state")? != "rejected"
        || row.try_get::<Option<String>, _>("ack_digest")?.as_deref() != Some(digest.as_str())
    {
        return Err(SessionError::Stale);
    }
    let existing: Option<String> = row.try_get("rejection_receipt_id")?;
    let receipt_id = if let Some(id) = existing {
        if row.try_get::<Option<i32>, _>("rejection_result")? != Some(ack.result) {
            return Err(SessionError::Invalid);
        }
        id
    } else {
        // Supports receipts for pre-upgrade rejected history, but only after the
        // original ACK digest proved its precise negative result and full Lease.
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("UPDATE worker_execution_attempts SET rejection_result=$2,rejection_receipt_id=$3 WHERE command_id=$1").bind(&ack.command_id).bind(ack.result).bind(&id).execute(&mut **tx).await?;
        super::execution::event(
            tx,
            &row.try_get::<String, _>("attempt_id")?,
            "rejection.receipt_created",
        )
        .await?;
        id
    };
    Ok(worker::RejectionReceipt {
        command_id: ack.command_id.clone(),
        payload_digest: ack.payload_digest.clone(),
        lease: ack.lease.clone(),
        result: ack.result,
        receipt_id,
    })
}
impl WorkerSessionStore {
    pub(crate) async fn query_rejection(
        &self,
        s: &DeviceSession,
        query: &worker::RejectionQuery,
    ) -> Result<worker::RejectionReceipt, SessionError> {
        if s.rejection_receipts_version != 1 {
            return Err(SessionError::Invalid);
        }
        let ack = query.rejected.as_ref().ok_or(SessionError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, s).await?;
        let result = receipt_in(&mut tx, s, ack).await?;
        finish(tx, s).await?;
        Ok(result)
    }
}
