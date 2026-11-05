//! Durable cloud execution barrier. Quote plans are trusted scheduler output;
//! this module exposes no caller-supplied model/job assignment endpoint. A grant
//! is permission to start, not proof of model execution, cleanup or settlement.
use super::{DeviceSession, SessionError, WorkerSessionStore, sessions::finish};
use orange_protocol::{
    SubmitJob,
    execution::{acknowledge_digest, assignment_digest},
    grpc::worker,
    request_digest,
};
use prost::Message;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn timestamp(seconds: i64) -> Option<prost_types::Timestamp> {
    Some(prost_types::Timestamp { seconds, nanos: 0 })
}
async fn event(tx: &mut Tx<'_>, attempt: &str, kind: &str) -> Result<(), SessionError> {
    sqlx::query("INSERT INTO worker_execution_events(event_id,attempt_id,kind) VALUES($1,$2,$3)")
        .bind(id())
        .bind(attempt)
        .bind(kind)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Control loss is uncertainty, never resource reclamation or a retry signal.
pub(super) async fn abandon_session(
    tx: &mut Tx<'_>,
    s: &DeviceSession,
) -> Result<(), SessionError> {
    let ids: Vec<String> = sqlx::query_scalar("UPDATE worker_execution_attempts SET state='unknown' WHERE node_id=$1 AND session_id=$2 AND connection_epoch=$3 AND state IN ('offered','started') RETURNING attempt_id")
        .bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).fetch_all(&mut **tx).await?;
    for attempt in ids {
        event(tx, &attempt, "session_lost_unknown").await?;
    }
    Ok(())
}
fn decode(row: &sqlx::postgres::PgRow) -> Result<worker::Assignment, SessionError> {
    let bytes: Vec<u8> = row.try_get("assignment")?;
    let a = worker::Assignment::decode(bytes.as_slice()).map_err(|_| SessionError::Invalid)?;
    if assignment_digest(&a).as_deref() != Some(a.payload_digest.as_str())
        || a.payload_digest != row.try_get::<String, _>("payload_digest")?
        || a.command_id != row.try_get::<String, _>("command_id")?
        || a.node_id != row.try_get::<String, _>("node_id")?
        || a.session_id != row.try_get::<String, _>("session_id")?
        || a.node_policy_generation != row.try_get::<i64, _>("policy_generation")? as u64
        || a.lease.as_ref().is_none_or(|l| {
            l.attempt_id != row.get::<String, _>("attempt_id")
                || l.job_id != row.get::<String, _>("job_id")
                || l.lease_id != row.get::<String, _>("lease_id")
                || l.connection_epoch != row.get::<i64, _>("connection_epoch") as u64
                || l.fencing_token != row.get::<i64, _>("fencing_token") as u64
        })
    {
        return Err(SessionError::Invalid);
    }
    Ok(a)
}
/// Root-before-Job agrees with reservation/cancellation; never takes wallet locks.
async fn job_authority(tx: &mut Tx<'_>, job: &str) -> Result<String, SessionError> {
    let row = sqlx::query("SELECT owner_id,root_budget_id FROM jobs WHERE job_id=$1")
        .bind(job)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(SessionError::Stale)?;
    let owner: String = row.try_get("owner_id")?;
    let root: String = row.try_get("root_budget_id")?;
    let active: Option<String> = sqlx::query_scalar(
        "SELECT owner_id FROM owners WHERE owner_id=$1 AND state='active' FOR SHARE",
    )
    .bind(&owner)
    .fetch_optional(&mut **tx)
    .await?;
    if active.is_none() {
        return Err(SessionError::Unauthorized);
    }
    let active: Option<String> = sqlx::query_scalar("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE").bind(root).bind(owner).fetch_optional(&mut **tx).await?;
    if active.is_none() {
        return Err(SessionError::Stale);
    }
    let row =
        sqlx::query("SELECT execution_state,settlement_state FROM jobs WHERE job_id=$1 FOR UPDATE")
            .bind(job)
            .fetch_one(&mut **tx)
            .await?;
    if row.try_get::<String, _>("settlement_state")? != "pending" {
        return Err(SessionError::Stale);
    }
    Ok(row.try_get("execution_state")?)
}
async fn eligible_plan(
    tx: &mut Tx<'_>,
    job: &str,
    node: &str,
) -> Result<sqlx::postgres::PgRow, SessionError> {
    let row = sqlx::query("SELECT p.*,j.root_budget_id,j.invocation_id,j.purpose,j.request_digest,r.model_version_id,r.model_sha256,r.execution_profile_digest,floor(extract(epoch FROM p.job_deadline))::bigint AS deadline FROM jobs j JOIN quotes q USING(quote_id) JOIN quote_execution_plans p USING(quote_id) JOIN reviewed_execution_profiles r USING(execution_profile_id) WHERE j.job_id=$1 AND p.target_node_id=$2 AND q.owner_id=j.owner_id AND q.root_budget_id=j.root_budget_id AND q.invocation_id=j.invocation_id AND q.max_amount=j.reserved AND p.request_projection_digest=q.projection_digest AND r.enabled AND p.job_deadline>clock_timestamp()+interval '1 second' FOR SHARE OF r")
        .bind(job).bind(node).fetch_optional(&mut **tx).await?.ok_or(SessionError::Stale)?;
    let submitted = SubmitJob {
        root_budget_id: row.try_get("root_budget_id")?,
        invocation_id: row.try_get("invocation_id")?,
        purpose: serde_json::from_value(serde_json::Value::String(row.try_get("purpose")?))
            .map_err(|_| SessionError::Invalid)?,
        quote_id: row.try_get("quote_id")?,
        request_projection_digest: row.try_get("request_projection_digest")?,
    };
    if request_digest("submit_job", &submitted).map_err(|_| SessionError::Invalid)?
        != row.try_get::<String, _>("request_digest")?
    {
        return Err(SessionError::Invalid);
    }
    Ok(row)
}
impl WorkerSessionStore {
    pub(crate) async fn next_execution(
        &self,
        s: &DeviceSession,
    ) -> Result<Option<worker::Assignment>, SessionError> {
        if s.execution_control_version == 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, s).await?;
        let (node_state, generation): (String, i64) =
            sqlx::query_as("SELECT state,policy_generation FROM nodes WHERE node_id=$1")
                .bind(&s.node_id)
                .fetch_one(&mut *tx)
                .await?;
        let old = sqlx::query("SELECT *,lease_expires_at>clock_timestamp() AS live,ack_deadline>clock_timestamp() AS ack_live,last_sent_at<=clock_timestamp()-interval '2 seconds' AS resend FROM worker_execution_attempts WHERE node_id=$1 AND state IN ('offered','started','unknown') FOR UPDATE").bind(&s.node_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = old {
            let state: String = row.try_get("state")?;
            let a = decode(&row)?;
            let current = row.try_get::<bool, _>("live")?
                && a.session_id == s.session_id
                && a.lease
                    .as_ref()
                    .is_some_and(|l| l.connection_epoch == s.connection_epoch as u64)
                && a.node_policy_generation == generation as u64
                && node_state == "online";
            if state != "unknown"
                && (!current || (state == "offered" && !row.try_get::<bool, _>("ack_live")?))
            {
                sqlx::query(
                    "UPDATE worker_execution_attempts SET state='unknown' WHERE attempt_id=$1",
                )
                .bind(row.try_get::<String, _>("attempt_id")?)
                .execute(&mut *tx)
                .await?;
                event(
                    &mut tx,
                    &row.try_get::<String, _>("attempt_id")?,
                    "authority_lost_unknown",
                )
                .await?;
            } else if state == "offered" && row.try_get::<bool, _>("resend")? {
                let job = row.try_get::<String, _>("job_id")?;
                if job_authority(&mut tx, &job).await? != "queued" {
                    return Err(SessionError::Stale);
                }
                eligible_plan(&mut tx, &job, &s.node_id).await?;
                sqlx::query("UPDATE worker_execution_attempts SET last_sent_at=clock_timestamp() WHERE attempt_id=$1").bind(row.try_get::<String,_>("attempt_id")?).execute(&mut *tx).await?;
                finish(tx, s).await?;
                return Ok(Some(a));
            }
            finish(tx, s).await?;
            return Ok(None);
        }
        if node_state != "online" {
            finish(tx, s).await?;
            return Ok(None);
        }
        let job: Option<String> = sqlx::query_scalar("SELECT j.job_id FROM jobs j JOIN quote_execution_plans p USING(quote_id) JOIN reviewed_execution_profiles r USING(execution_profile_id) JOIN quotes q USING(quote_id) JOIN owners o ON o.owner_id=j.owner_id JOIN budget_roots b ON b.root_budget_id=j.root_budget_id WHERE p.target_node_id=$1 AND j.execution_state='queued' AND j.settlement_state='pending' AND r.enabled AND o.state='active' AND b.state='active' AND p.request_projection_digest=q.projection_digest AND p.job_deadline>clock_timestamp()+interval '11 seconds' AND NOT EXISTS(SELECT 1 FROM worker_execution_attempts a WHERE a.job_id=j.job_id) ORDER BY j.job_id LIMIT 1")
            .bind(&s.node_id).fetch_optional(&mut *tx).await?;
        let Some(job) = job else {
            finish(tx, s).await?;
            return Ok(None);
        };
        if job_authority(&mut tx, &job).await? != "queued" {
            return Err(SessionError::Stale);
        }
        let plan = eligible_plan(&mut tx, &job, &s.node_id).await?;
        let attempt = id();
        let command = id();
        let lease = id();
        let row = sqlx::query("INSERT INTO worker_execution_attempts(attempt_id,job_id,command_id,node_id,session_id,connection_epoch,policy_generation,lease_id,lease_expires_at,ack_deadline,payload_digest,assignment) VALUES($1,$2,$3,$4,$5,$6,$7,$8,LEAST(date_trunc('second',clock_timestamp()+interval '45 seconds'),to_timestamp($9)),date_trunc('second',clock_timestamp()+interval '10 seconds'),$10,$11) RETURNING fencing_token,floor(extract(epoch FROM lease_expires_at))::bigint AS expires")
            .bind(&attempt).bind(&job).bind(&command).bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).bind(generation).bind(&lease).bind(plan.try_get::<i64,_>("deadline")? as f64).bind("0".repeat(64)).bind(Vec::<u8>::new()).fetch_one(&mut *tx).await?;
        let mut a = worker::Assignment {
            command_id: command,
            payload_digest: String::new(),
            lease: Some(worker::Lease {
                lease_id: lease,
                job_id: job,
                attempt_id: attempt.clone(),
                connection_epoch: s.connection_epoch as u64,
                fencing_token: row.try_get::<i64, _>("fencing_token")? as u64,
                expires_at: timestamp(row.try_get("expires")?),
            }),
            model_version_id: plan.try_get("model_version_id")?,
            execution_profile_id: plan.try_get("execution_profile_id")?,
            request_projection_digest: plan.try_get("request_projection_digest")?,
            input_artifact_ids: plan.try_get("input_artifact_ids")?,
            job_deadline: timestamp(plan.try_get("deadline")?),
            node_id: s.node_id.clone(),
            session_id: s.session_id.clone(),
            node_policy_generation: generation as u64,
            model_sha256: plan.try_get("model_sha256")?,
            execution_profile_digest: plan.try_get("execution_profile_digest")?,
        };
        a.payload_digest = assignment_digest(&a).ok_or(SessionError::Invalid)?;
        sqlx::query("UPDATE worker_execution_attempts SET payload_digest=$2,assignment=$3 WHERE attempt_id=$1").bind(&attempt).bind(&a.payload_digest).bind(a.encode_to_vec()).execute(&mut *tx).await?;
        event(&mut tx, &attempt, "assignment.offered").await?;
        finish(tx, s).await?;
        Ok(Some(a))
    }
    pub(crate) async fn acknowledge_execution(
        &self,
        s: &DeviceSession,
        ack: &worker::Acknowledge,
    ) -> Result<Option<worker::StartGrant>, SessionError> {
        if s.execution_control_version != 1 {
            return Err(SessionError::Invalid);
        }
        let lease = ack.lease.as_ref().ok_or(SessionError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, s).await?;
        let (state, generation): (String, i64) =
            sqlx::query_as("SELECT state,policy_generation FROM nodes WHERE node_id=$1")
                .bind(&s.node_id)
                .fetch_one(&mut *tx)
                .await?;
        if state != "online" {
            return Err(SessionError::Stale);
        }
        // Scope lookup before accessing a Job: another device cannot use guessed
        // IDs to acquire unrelated owner/budget locks or authorization information.
        let job:Option<String> = sqlx::query_scalar("SELECT job_id FROM worker_execution_attempts WHERE command_id=$1 AND node_id=$2 AND session_id=$3 AND connection_epoch=$4").bind(&ack.command_id).bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).fetch_optional(&mut *tx).await?;
        let job = job.ok_or(SessionError::Stale)?;
        if job != lease.job_id {
            return Err(SessionError::Invalid);
        }
        let job_state = job_authority(&mut tx, &job).await?;
        eligible_plan(&mut tx, &job, &s.node_id).await?;
        let row = sqlx::query("SELECT *,lease_expires_at>clock_timestamp() AS live,ack_deadline>clock_timestamp() AS ack_live FROM worker_execution_attempts WHERE command_id=$1 FOR UPDATE").bind(&ack.command_id).fetch_one(&mut *tx).await?;
        let a = decode(&row)?;
        if a.node_policy_generation != generation as u64 || !row.try_get::<bool, _>("live")? {
            return Err(SessionError::Stale);
        }
        let digest = acknowledge_digest(ack, &a).ok_or(SessionError::Invalid)?;
        let previous: Option<String> = row.try_get("ack_digest")?;
        let state: String = row.try_get("state")?;
        let accepted = ack.result == worker::AckResult::Accepted as i32;
        if let Some(previous) = previous {
            if previous != digest {
                return Err(SessionError::Invalid);
            }
            if !((state == "started" && job_state == "running" && accepted)
                || (state == "rejected" && job_state == "queued" && !accepted))
            {
                return Err(SessionError::Stale);
            }
        } else {
            if state != "offered" || job_state != "queued" || !row.try_get::<bool, _>("ack_live")? {
                return Err(SessionError::Stale);
            }
            sqlx::query("UPDATE worker_execution_attempts SET state=$2,ack_digest=$3,execution_journal_id=$4,broker_grant_id=$5,broker_epoch=$6,slot_generation=$7 WHERE attempt_id=$1")
                .bind(&lease.attempt_id).bind(if accepted {"started"} else {"rejected"}).bind(&digest)
                .bind(accepted.then_some(&ack.execution_journal_id)).bind(accepted.then_some(&ack.broker_grant_id))
                .bind(accepted.then_some(ack.broker_epoch as i64)).bind(accepted.then_some(ack.slot_generation as i64)).execute(&mut *tx).await?;
            if accepted {
                let owner: String = sqlx::query_scalar(
                    "UPDATE jobs SET execution_state='running' WHERE job_id=$1 RETURNING owner_id",
                )
                .bind(&job)
                .fetch_one(&mut *tx)
                .await?;
                crate::events::append(&mut tx, &owner, &job, "job.start_granted", &digest).await?;
            }
            event(
                &mut tx,
                &lease.attempt_id,
                if accepted {
                    "start.granted"
                } else {
                    "assignment.rejected"
                },
            )
            .await?;
            let live:bool = sqlx::query_scalar("SELECT ack_deadline>clock_timestamp() AND lease_expires_at>clock_timestamp() FROM worker_execution_attempts WHERE attempt_id=$1").bind(&lease.attempt_id).fetch_one(&mut *tx).await?;
            if !live {
                return Err(SessionError::Stale);
            }
        }
        // Also recheck on an idempotent ACK replay; it must not extend a grant
        // because a transaction waited for another authority lock.
        let live: bool = sqlx::query_scalar("SELECT lease_expires_at>clock_timestamp() FROM worker_execution_attempts WHERE attempt_id=$1")
            .bind(&lease.attempt_id).fetch_one(&mut *tx).await?;
        if !live {
            return Err(SessionError::Stale);
        }
        finish(tx, s).await?;
        Ok(accepted.then(|| worker::StartGrant {
            command_id: ack.command_id.clone(),
            lease: ack.lease.clone(),
            payload_digest: ack.payload_digest.clone(),
            execution_journal_id: ack.execution_journal_id.clone(),
            broker_grant_id: ack.broker_grant_id.clone(),
            broker_epoch: ack.broker_epoch,
            slot_generation: ack.slot_generation,
        }))
    }
}
