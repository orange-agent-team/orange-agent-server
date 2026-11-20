//! Request-bound current authority; duplicates never extend execution time.
use super::{DeviceSession, SessionError, WorkerSessionStore, execution, sessions};
use orange_protocol::{
    execution::{acknowledge_digest, renewal_digest},
    grpc::worker,
};
use sqlx::Row;
impl WorkerSessionStore {
    pub(crate) async fn renew_execution(
        &self,
        s: &DeviceSession,
        r: &worker::ExecutionRenewalRequest,
    ) -> Result<worker::ExecutionRenewalReply, SessionError> {
        if s.renewal_version != 1
            || r.session_id != s.session_id
            || r.sequence == 0
            || r.sequence > 8640
        {
            return Err(SessionError::Invalid);
        }
        let ack = r.accepted.as_ref().ok_or(SessionError::Invalid)?;
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
        let job: Option<String> = sqlx::query_scalar("SELECT job_id FROM worker_execution_attempts WHERE command_id=$1 AND node_id=$2 AND session_id=$3 AND connection_epoch=$4").bind(&ack.command_id).bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).fetch_optional(&mut *tx).await?;
        if job.as_deref() != Some(lease.job_id.as_str()) {
            return Err(SessionError::Stale);
        }
        if execution::job_authority(&mut tx, &lease.job_id).await? != "running" {
            return Err(SessionError::Stale);
        }
        let plan = execution::eligible_plan(&mut tx, &lease.job_id, &s.node_id).await?;
        let (text, audio) = execution::checked_assignment_inputs(
            &mut tx,
            s,
            &lease.job_id,
            sqlx::Row::try_get(&plan, "purpose")?,
        )
        .await?;
        if text.is_none() && audio.is_none() {
            return Err(SessionError::Stale);
        }
        let row = sqlx::query("SELECT *,lease_expires_at>clock_timestamp() AS live,extract(epoch FROM lease_expires_at)::double precision AS prior_expires FROM worker_execution_attempts WHERE command_id=$1 FOR UPDATE").bind(&ack.command_id).fetch_one(&mut *tx).await?;
        let a = execution::decode(&row)?;
        if a.transcription_input != audio {
            return Err(SessionError::Stale);
        }
        let digest = renewal_digest(r, &a).ok_or(SessionError::Invalid)?;
        if a.node_policy_generation != generation as u64
            || !row.try_get::<bool, _>("live")?
            || row.try_get::<String, _>("state")? != "started"
            || row.try_get::<Option<String>, _>("ack_digest")? != acknowledge_digest(ack, &a)
            || row
                .try_get::<Option<String>, _>("supervisor_instance_id")?
                .is_some_and(|v| v != r.supervisor_instance_id)
        {
            return Err(SessionError::Stale);
        }
        let previous = sqlx::query("SELECT request_digest,sequence,budget_ms,floor(extract(epoch FROM expires_at))::bigint AS expires,expires_at>clock_timestamp() AS live FROM worker_execution_renewals WHERE attempt_id=$1 AND request_id=$2").bind(&lease.attempt_id).bind(&r.request_id).fetch_optional(&mut *tx).await?;
        let is_new = previous.is_none();
        let prior_expires: f64 = row.try_get("prior_expires")?;
        let (budget, expires) = if let Some(old) = previous {
            if old.try_get::<String, _>("request_digest")? != digest
                || old.try_get::<i64, _>("sequence")? != r.sequence as i64
                || !old.try_get::<bool, _>("live")?
            {
                return Err(SessionError::Stale);
            }
            (
                old.try_get::<i64, _>("budget_ms")?,
                old.try_get::<i64, _>("expires")?,
            )
        } else {
            if row.try_get::<i64, _>("renewal_sequence")?.checked_add(1) != Some(r.sequence as i64)
            {
                return Err(SessionError::Stale);
            }
            let rate:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM worker_execution_renewals WHERE attempt_id=$1 AND created_at>clock_timestamp()-interval '5 seconds')").bind(&lease.attempt_id).fetch_one(&mut *tx).await?;
            if !rate {
                return Err(SessionError::Stale);
            }
            let timing=sqlx::query("SELECT floor(extract(epoch FROM until_time))::bigint AS expires,floor(extract(epoch FROM (until_time-clock_timestamp()))*1000)::bigint AS budget FROM (SELECT LEAST(date_trunc('second',clock_timestamp()+interval '45 seconds'),p.job_deadline,s.expires_at) AS until_time FROM jobs j JOIN quote_execution_plans p USING(quote_id) JOIN worker_sessions s ON s.node_id=$2 WHERE j.job_id=$1) bound").bind(&lease.job_id).bind(&s.node_id).fetch_one(&mut *tx).await?;
            let budget: i64 = timing.try_get("budget")?;
            let expires: i64 = timing.try_get("expires")?;
            if !(2000..=45000).contains(&budget) {
                return Err(SessionError::Stale);
            }
            // Keep prior expiry live until this write; no revive after DB waits.
            let changed=sqlx::query("UPDATE worker_execution_attempts SET lease_expires_at=to_timestamp($2),renewal_sequence=$3,supervisor_instance_id=$4 WHERE attempt_id=$1 AND lease_expires_at>clock_timestamp() AND state='started'").bind(&lease.attempt_id).bind(expires as f64).bind(r.sequence as i64).bind(&r.supervisor_instance_id).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(SessionError::Stale);
            }
            sqlx::query("INSERT INTO worker_execution_renewals(attempt_id,request_id,sequence,request_digest,budget_ms,expires_at) VALUES($1,$2,$3,$4,$5,to_timestamp($6))").bind(&lease.attempt_id).bind(&r.request_id).bind(r.sequence as i64).bind(&digest).bind(budget).bind(expires as f64).execute(&mut *tx).await?;
            execution::event(&mut tx, &lease.attempt_id, "execution.renewed").await?;
            (budget, expires)
        };
        let continuous: bool = sqlx::query_scalar("SELECT clock_timestamp()<to_timestamp($1)")
            .bind(prior_expires)
            .fetch_one(&mut *tx)
            .await?;
        if !continuous {
            return Err(SessionError::Stale);
        }
        sessions::finish(tx, s).await?;
        // A committed historical decision alone is not current permission.
        self.check(s).await?;
        let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_execution_attempts a JOIN nodes n USING(node_id) JOIN worker_execution_renewals r USING(attempt_id) WHERE a.attempt_id=$1 AND a.state='started' AND a.session_id=$2 AND a.policy_generation=n.policy_generation AND n.state='online' AND a.lease_expires_at>clock_timestamp() AND r.request_id=$3 AND r.expires_at>clock_timestamp())").bind(&lease.attempt_id).bind(&s.session_id).bind(&r.request_id).fetch_one(&self.pool).await?;
        let continuous: bool = sqlx::query_scalar("SELECT clock_timestamp()<to_timestamp($1)")
            .bind(prior_expires)
            .fetch_one(&self.pool)
            .await?;
        if is_new && !continuous {
            // Commit may itself stall beyond old authority. Preserve the fact,
            // quarantine the attempt, and never manufacture a resumed lease.
            let mut fence = self.pool.begin().await?;
            let changed=sqlx::query("UPDATE worker_execution_attempts SET state='unknown' WHERE attempt_id=$1 AND state='started'").bind(&lease.attempt_id).execute(&mut *fence).await?.rows_affected();
            if changed == 1 {
                execution::event(
                    &mut fence,
                    &lease.attempt_id,
                    "renewal.commit_expired_unknown",
                )
                .await?;
            }
            fence.commit().await?;
            return Err(SessionError::Stale);
        }
        if !live || !continuous {
            return Err(SessionError::Stale);
        }
        Ok(worker::ExecutionRenewalReply {
            request_id: r.request_id.clone(),
            sequence: r.sequence,
            request_digest: digest,
            budget_ms: budget as u64,
            expires_at: Some(prost_types::Timestamp {
                seconds: expires,
                nanos: 0,
            }),
        })
    }
}
