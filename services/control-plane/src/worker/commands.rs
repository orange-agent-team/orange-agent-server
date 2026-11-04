//! Transactional node-command delivery. A local receipt leaves the administrative
//! action executing; there is deliberately no resource-release transition here.
use super::{DeviceSession, SessionError, WorkerSessionStore, sessions::finish};
use orange_protocol::{
    grpc::worker,
    node_control::{NodeStopIntent, StopKind},
    valid_digest, valid_id,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

async fn audit(
    tx: &mut Tx<'_>,
    s: &DeviceSession,
    command: &str,
    kind: &str,
    receipt: Option<&str>,
) -> Result<(), SessionError> {
    sqlx::query("INSERT INTO worker_node_command_events(event_id,node_id,command_id,session_id,connection_epoch,certificate_sha256,kind,receipt_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&s.node_id).bind(command).bind(&s.session_id).bind(s.connection_epoch).bind(&s.certificate_sha256).bind(kind).bind(receipt).execute(&mut **tx).await?;
    Ok(())
}
async fn uncertain_action(tx: &mut Tx<'_>, action: &str, state: &str) -> Result<(), SessionError> {
    let row=sqlx::query("SELECT a.document,c.revision FROM admin_actions a JOIN admin_action_control c USING(action_id) WHERE action_id=$1 FOR UPDATE OF a,c").bind(action).fetch_one(&mut **tx).await?;
    let mut doc: Value = row.try_get("document")?;
    if doc["state"] != "executing" {
        return Ok(());
    }
    let version = row
        .try_get::<i64, _>("revision")?
        .checked_add(1)
        .ok_or(SessionError::Stale)?;
    doc["state"] = json!(state);
    doc["version"] = json!(format!("v{version}"));
    sqlx::query("UPDATE admin_actions SET document=$2 WHERE action_id=$1")
        .bind(action)
        .bind(doc)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE admin_action_control SET revision=$2 WHERE action_id=$1")
        .bind(action)
        .bind(version)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub(super) async fn abandon_dispatched(
    tx: &mut Tx<'_>,
    s: &DeviceSession,
) -> Result<(), SessionError> {
    let rows=sqlx::query("UPDATE admin_node_commands SET state='unknown' WHERE node_id=$1 AND state='dispatched' AND dispatch_session_id=$2 AND dispatch_connection_epoch=$3 RETURNING command_id,action_id")
        .bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).fetch_all(&mut **tx).await?;
    for row in rows {
        uncertain_action(tx, &row.try_get::<String, _>("action_id")?, "unknown").await?;
        audit(
            tx,
            s,
            &row.try_get::<String, _>("command_id")?,
            "command.disconnected_unknown",
            None,
        )
        .await?;
    }
    Ok(())
}
impl WorkerSessionStore {
    pub(crate) async fn next_node_command(
        &self,
        s: &DeviceSession,
    ) -> Result<Option<worker::NodeCommand>, SessionError> {
        if !matches!(s.node_control_version, 1 | 2) {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, s).await?;
        let (generation, node_state): (i64, String) =
            sqlx::query_as("SELECT policy_generation,state FROM nodes WHERE node_id=$1")
                .bind(&s.node_id)
                .fetch_one(&mut *tx)
                .await?;
        // Bound each maintenance batch. Commands are never replayed after an
        // uncertain send; recovery must query the original local receipt.
        let obsolete=sqlx::query("SELECT command_id,action_id,state FROM admin_node_commands WHERE node_id=$1 AND ((state IN('queued','received') AND policy_generation<$2) OR (state='dispatched' AND (policy_generation<$2 OR dispatch_session_id IS DISTINCT FROM $3 OR dispatch_connection_epoch IS DISTINCT FROM $4 OR dispatched_at<=clock_timestamp()-interval '15 seconds'))) ORDER BY policy_generation LIMIT 64 FOR UPDATE")
            .bind(&s.node_id).bind(generation).bind(&s.session_id).bind(s.connection_epoch).fetch_all(&mut *tx).await?;
        for row in obsolete {
            let command: String = row.try_get("command_id")?;
            let state = if row.try_get::<String, _>("state")? == "queued" {
                "failed"
            } else {
                "unknown"
            };
            sqlx::query("UPDATE admin_node_commands SET state=$2 WHERE command_id=$1")
                .bind(&command)
                .bind(state)
                .execute(&mut *tx)
                .await?;
            uncertain_action(&mut tx, &row.try_get::<String, _>("action_id")?, state).await?;
            audit(
                &mut tx,
                s,
                &command,
                if state == "failed" {
                    "command.superseded_unsent"
                } else {
                    "command.delivery_unknown"
                },
                None,
            )
            .await?;
        }
        let row=sqlx::query("SELECT * FROM admin_node_commands WHERE node_id=$1 AND policy_generation=$2 AND state='queued' FOR UPDATE").bind(&s.node_id).bind(generation).fetch_optional(&mut *tx).await?;
        let result = if let Some(row) = row {
            let kind = match row.try_get::<String, _>("command_kind")?.as_str() {
                "node.drain" if node_state == "draining" => StopKind::Drain,
                "node.quarantine" if node_state == "quarantined" => StopKind::Quarantine,
                _ => return Err(SessionError::Stale),
            };
            let intent = NodeStopIntent {
                command_id: row.try_get("command_id")?,
                action_id: row.try_get("action_id")?,
                node_id: s.node_id.clone(),
                policy_generation: generation.to_string(),
                kind,
            };
            if !intent.validate() {
                return Err(SessionError::Invalid);
            }
            let digest = intent.digest().map_err(|_| SessionError::Invalid)?;
            if row
                .try_get::<Option<String>, _>("payload_digest")?
                .is_some_and(|stored| stored != digest)
            {
                return Err(SessionError::Invalid);
            }
            sqlx::query("UPDATE admin_node_commands SET state='dispatched',payload_digest=$2,dispatch_session_id=$3,dispatch_connection_epoch=$4,dispatched_at=clock_timestamp() WHERE command_id=$1")
                .bind(&intent.command_id).bind(&digest).bind(&s.session_id).bind(s.connection_epoch).execute(&mut *tx).await?;
            audit(&mut tx, s, &intent.command_id, "command.dispatched", None).await?;
            Some(worker::NodeCommand {
                command_id: intent.command_id,
                action_id: intent.action_id,
                node_id: s.node_id.clone(),
                policy_generation: generation as u64,
                kind: match kind {
                    StopKind::Drain => worker::NodeStopKind::Drain as i32,
                    StopKind::Quarantine => worker::NodeStopKind::Quarantine as i32,
                },
                payload_digest: digest,
                session_id: s.session_id.clone(),
                connection_epoch: s.connection_epoch as u64,
            })
        } else {
            None
        };
        finish(tx, s).await?;
        Ok(result)
    }
    pub(crate) async fn receive_node_receipt(
        &self,
        s: &DeviceSession,
        receipt: &worker::NodeReceipt,
    ) -> Result<(), SessionError> {
        if !matches!(s.node_control_version, 1 | 2)
            || receipt.node_id != s.node_id
            || receipt.session_id != s.session_id
            || i64::try_from(receipt.connection_epoch).ok() != Some(s.connection_epoch)
            || !valid_id(&receipt.command_id)
            || !valid_id(&receipt.receipt_id)
            || !valid_digest(&receipt.payload_digest)
        {
            return Err(SessionError::Invalid);
        }
        let generation = i64::try_from(receipt.policy_generation)
            .ok()
            .filter(|g| *g > 0)
            .ok_or(SessionError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, s).await?;
        let current_generation: i64 =
            sqlx::query_scalar("SELECT policy_generation FROM nodes WHERE node_id=$1")
                .bind(&s.node_id)
                .fetch_one(&mut *tx)
                .await?;
        if current_generation != generation {
            return Err(SessionError::Stale);
        }
        let row=sqlx::query("SELECT *,dispatched_at>clock_timestamp()-interval '15 seconds' AS timely FROM admin_node_commands WHERE command_id=$1 AND node_id=$2 AND policy_generation=$3 AND dispatch_session_id=$4 AND dispatch_connection_epoch=$5 AND payload_digest=$6 FOR UPDATE")
            .bind(&receipt.command_id).bind(&s.node_id).bind(generation).bind(&s.session_id).bind(s.connection_epoch).bind(&receipt.payload_digest).fetch_optional(&mut *tx).await?.ok_or(SessionError::Stale)?;
        let state: String = row.try_get("state")?;
        if state == "received" {
            if row.try_get::<Option<String>, _>("receipt_id")?.as_deref()
                != Some(receipt.receipt_id.as_str())
            {
                return Err(SessionError::Invalid);
            }
        } else {
            if state != "dispatched" || row.try_get::<Option<bool>, _>("timely")? != Some(true) {
                return Err(SessionError::Stale);
            }
            sqlx::query("UPDATE admin_node_commands SET state='received',receipt_id=$2,received_at=clock_timestamp() WHERE command_id=$1").bind(&receipt.command_id).bind(&receipt.receipt_id).execute(&mut *tx).await?;
            audit(
                &mut tx,
                s,
                &receipt.command_id,
                "command.received",
                Some(&receipt.receipt_id),
            )
            .await?;
            let timely:bool=sqlx::query_scalar("SELECT dispatched_at>clock_timestamp()-interval '15 seconds' FROM admin_node_commands WHERE command_id=$1").bind(&receipt.command_id).fetch_one(&mut *tx).await?;
            if !timely {
                return Err(SessionError::Stale);
            }
        }
        finish(tx, s).await
    }
}
