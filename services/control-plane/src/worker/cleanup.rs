use super::{DeviceSession, SessionError, WorkerSessionStore, execution, sessions};
use orange_protocol::{
    cleanup::{CpuCleanupEvidence, cleanup_digest},
    execution::acknowledge_digest,
    grpc::worker,
    request_digest,
};
use prost::Message;
use sqlx::Row;
impl WorkerSessionStore {
    pub(crate) async fn confirm_cpu_cleanup(
        &self,
        s: &DeviceSession,
        r: &worker::CpuCleanupRequest,
    ) -> Result<worker::CpuCleanupReceipt, SessionError> {
        if s.cpu_cleanup_version != 1 || r.session_id != s.session_id {
            return Err(SessionError::Invalid);
        }
        let ack = r.accepted.as_ref().ok_or(SessionError::Invalid)?;
        let evidence = CpuCleanupEvidence::parse(&r.evidence_json).ok_or(SessionError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        self.authorize_in(&mut tx, s).await?;
        let scope=sqlx::query("SELECT a.job_id,j.root_budget_id,j.owner_id FROM worker_execution_attempts a JOIN jobs j USING(job_id) WHERE a.command_id=$1 AND a.node_id=$2").bind(&ack.command_id).bind(&s.node_id).fetch_optional(&mut *tx).await?.ok_or(SessionError::Unauthorized)?;
        let job: String = scope.try_get("job_id")?;
        let owner: String = scope.try_get("owner_id")?;
        sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 FOR SHARE")
            .bind(scope.try_get::<String, _>("root_budget_id")?)
            .fetch_one(&mut *tx)
            .await?;
        let jobrow = sqlx::query("SELECT * FROM jobs WHERE job_id=$1 FOR UPDATE")
            .bind(&job)
            .fetch_one(&mut *tx)
            .await?;
        let row =
            sqlx::query("SELECT * FROM worker_execution_attempts WHERE command_id=$1 FOR UPDATE")
                .bind(&ack.command_id)
                .fetch_one(&mut *tx)
                .await?;
        let a = execution::decode(&row)?;
        let digest = cleanup_digest(ack, &a, &evidence, &r.outcome).ok_or(SessionError::Invalid)?;
        if digest != r.request_digest
            || row.try_get::<Option<String>, _>("ack_digest")? != acknowledge_digest(ack, &a)
        {
            return Err(SessionError::Invalid);
        }
        if row
            .try_get::<Option<String>, _>("supervisor_instance_id")?
            .is_some_and(|v| v != evidence.grant.supervisor_instance)
        {
            return Err(SessionError::Invalid);
        }
        let lease = a.lease.as_ref().ok_or(SessionError::Invalid)?;
        let prior = sqlx::query(
            "SELECT request_digest,receipt FROM worker_cpu_cleanup_receipts WHERE attempt_id=$1",
        )
        .bind(&lease.attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(prior) = prior {
            if prior.try_get::<String, _>("request_digest")? != digest
                || row.try_get::<String, _>("state")? != "closed"
            {
                return Err(SessionError::Stale);
            }
            let bytes: Vec<u8> = prior.try_get("receipt")?;
            let result = worker::CpuCleanupReceipt::decode(bytes.as_slice())
                .map_err(|_| SessionError::Invalid)?;
            sessions::finish(tx, s).await?;
            return Ok(result);
        }
        if !matches!(
            row.try_get::<String, _>("state")?.as_str(),
            "started" | "unknown"
        ) {
            return Err(SessionError::Stale);
        }
        let profile=sqlx::query("SELECT p.*,r.model_version_id,r.model_sha256,r.execution_profile_digest FROM reviewed_cpu_profiles p JOIN reviewed_execution_profiles r USING(execution_profile_id) WHERE p.execution_profile_id=$1 FOR SHARE OF p,r").bind(&a.execution_profile_id).fetch_optional(&mut *tx).await?.ok_or(SessionError::Unauthorized)?;
        let execution_digest=request_digest("orange-owned-cpu-v1",&serde_json::json!({"backend_sha256":profile.try_get::<String,_>("executable_sha256")?,"model_sha256":profile.try_get::<String,_>("model_sha256")?,"model_bytes":profile.try_get::<i64,_>("model_bytes")?.to_string(),"model_version":profile.try_get::<String,_>("model_version_id")?,"context_tokens":profile.try_get::<i32,_>("context_tokens")?,"startup_timeout_ms":profile.try_get::<i64,_>("startup_timeout_ms")?.to_string(),"gpu_layers":0,"parallel":1,"threads":2})).map_err(|_|SessionError::Invalid)?;
        if execution_digest != a.execution_profile_digest
            || execution_digest != profile.try_get::<String, _>("execution_profile_digest")?
            || evidence.process.executable_sha256
                != profile.try_get::<String, _>("executable_sha256")?
        {
            return Err(SessionError::Invalid);
        }
        let response: Option<String> =
            sqlx::query_scalar("SELECT commit_id FROM job_response_commits WHERE job_id=$1")
                .bind(&job)
                .fetch_optional(&mut *tx)
                .await?;
        let outcome = if response.is_some() {
            if jobrow.try_get::<String, _>("execution_state")? != "succeeded" {
                return Err(SessionError::Stale);
            }
            "succeeded"
        } else {
            if r.outcome == "delivered"
                || !matches!(
                    jobrow.try_get::<String, _>("execution_state")?.as_str(),
                    "running" | "cancelling" | "failed" | "cancelled"
                )
            {
                return Err(SessionError::Stale);
            }
            if r.outcome == "cancelled"
                || jobrow.try_get::<String, _>("execution_state")? == "cancelling"
            {
                "cancelled"
            } else {
                "failed"
            }
        };
        let receipt = worker::CpuCleanupReceipt {
            receipt_id: uuid::Uuid::new_v4().to_string(),
            command_id: a.command_id.clone(),
            attempt_id: lease.attempt_id.clone(),
            job_id: job.clone(),
            request_digest: digest.clone(),
            outcome: outcome.into(),
            response_commit_id: response.clone().unwrap_or_default(),
        };
        sqlx::query("INSERT INTO worker_cpu_cleanup_receipts(attempt_id,job_id,node_id,model_instance_id,request_digest,receipt_id,receipt,evidence) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(&lease.attempt_id).bind(&job).bind(&s.node_id).bind(&evidence.process.instance_id).bind(&digest).bind(&receipt.receipt_id).bind(receipt.encode_to_vec()).bind(serde_json::to_value(&evidence).map_err(|_|SessionError::Invalid)?).execute(&mut *tx).await?;
        sqlx::query("UPDATE worker_execution_attempts SET state='closed' WHERE attempt_id=$1")
            .bind(&lease.attempt_id)
            .execute(&mut *tx)
            .await?;
        if response.is_none() {
            sqlx::query("UPDATE jobs SET execution_state=$2 WHERE job_id=$1")
                .bind(&job)
                .bind(outcome)
                .execute(&mut *tx)
                .await?;
            crate::events::append(
                &mut tx,
                &owner,
                &job,
                if outcome == "failed" {
                    "job.failed"
                } else {
                    "job.cancelled"
                },
                &digest,
            )
            .await?;
        }
        execution::event(&mut tx, &lease.attempt_id, "cpu.cleanup_confirmed").await?;
        sessions::finish(tx, s).await?;
        Ok(receipt)
    }
}
