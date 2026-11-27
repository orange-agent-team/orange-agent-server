//! A short-lived URL is scoped to a single started lease and its current bound input.
//! The control stream, grant and post-download check have independent lifetimes.
use super::*;
use crate::worker::{DeviceSession, WorkerSessionStore};
use orange_protocol::grpc::worker;

impl ArtifactReader {
    async fn direct_input_scope(
        &self,
        tx: &mut Tx<'_>,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: &worker::InputDownloadRequest,
    ) -> Result<(worker::InputArtifact, String), ArtifactError> {
        if r.protocol_version != 1 || !valid_id(&r.command_id) {
            return Err(ArtifactError::Invalid);
        }
        let lease = r.lease.as_ref().ok_or(ArtifactError::Invalid)?;
        let input = r.artifact.as_ref().ok_or(ArtifactError::Invalid)?;
        let assignment = self
            .started_assignment(tx, sessions, s, &r.session_id, lease)
            .await?;
        if assignment.command_id != r.command_id {
            return Err(ArtifactError::Forbidden);
        }
        let (bound, key) = self
            .bound_input(
                tx,
                &assignment,
                lease,
                &input.artifact_id,
                &input.version_id,
            )
            .await?;
        if &bound != input
            || bound.media_type != "application/json"
            || bound.size_bytes == 0
            || bound.size_bytes > 256 * 1024
        {
            return Err(ArtifactError::Forbidden);
        }
        Ok((bound, key))
    }

    pub(crate) async fn authorize_input_download(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        r: worker::InputDownloadRequest,
    ) -> Result<worker::InputDownloadReply, ArtifactError> {
        let _slot = self
            .store
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let mut tx = self.store.inner.pool.begin().await?;
        let (input, key) = self.direct_input_scope(&mut tx, sessions, s, &r).await?;
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let remote = self.store.inner.objects.remote()?;
        let provider = remote.checked_version(&key, input.size_bytes).await;
        let mut tx = self.store.inner.pool.begin().await?;
        let (live, live_key) = self.direct_input_scope(&mut tx, sessions, s, &r).await?;
        if live != input || live_key != key {
            return Err(ArtifactError::Integrity);
        }
        let provider = provider?;
        let lease = r.lease.as_ref().ok_or(ArtifactError::Invalid)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ArtifactError::Unavailable)?
            .as_secs();
        // Durable lease expiry incorporates only accepted renewals. Old epoch, fence, session,
        // input version or projection has already been rejected by started_assignment.
        let limit:i64=sqlx::query_scalar("SELECT LEAST(floor(extract(epoch FROM least(a.lease_expires_at,ws.expires_at,c.expires_at,p.job_deadline)))::bigint,
                floor(extract(epoch FROM clock_timestamp()))::bigint+60,$4)
            FROM worker_execution_attempts a JOIN worker_sessions ws ON ws.session_id=a.session_id
            JOIN worker_credentials c ON c.certificate_sha256=ws.certificate_sha256 JOIN jobs j ON j.job_id=a.job_id JOIN quote_execution_plans p USING(quote_id)
            WHERE a.attempt_id=$1 AND a.session_id=$2 AND ws.certificate_sha256=$3 AND a.state='started' AND c.active")
            .bind(&lease.attempt_id).bind(&s.session_id).bind(&s.certificate_sha256).bind(i64::try_from(now+60).map_err(|_|ArtifactError::Integrity)?)
            .fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        if !r.verify_only {
            sqlx::query("INSERT INTO worker_input_download_grants(attempt_id,artifact_id,version_id,session_id,node_id,certificate_sha256,command_id,lease_id,
                connection_epoch,fencing_token,provider_version,expires_unix) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT DO NOTHING")
                .bind(&lease.attempt_id).bind(&input.artifact_id).bind(&input.version_id).bind(&s.session_id).bind(&s.node_id).bind(&s.certificate_sha256)
                .bind(&r.command_id).bind(&lease.lease_id).bind(s.connection_epoch).bind(i64::try_from(lease.fencing_token).map_err(|_|ArtifactError::Invalid)?)
                .bind(&provider).bind(limit).execute(&mut *tx).await?;
        }
        let cutoff:i64=sqlx::query_scalar("SELECT LEAST(expires_unix,$12) FROM worker_input_download_grants WHERE attempt_id=$1 AND artifact_id=$2 AND version_id=$3
            AND session_id=$4 AND node_id=$5 AND certificate_sha256=$6 AND command_id=$7 AND lease_id=$8 AND connection_epoch=$9 AND fencing_token=$10 AND provider_version=$11")
            .bind(&lease.attempt_id).bind(&input.artifact_id).bind(&input.version_id).bind(&s.session_id).bind(&s.node_id).bind(&s.certificate_sha256)
            .bind(&r.command_id).bind(&lease.lease_id).bind(s.connection_epoch).bind(i64::try_from(lease.fencing_token).map_err(|_|ArtifactError::Invalid)?)
            .bind(&provider).bind(limit).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
        let until = u64::try_from(cutoff).map_err(|_| ArtifactError::Expired)?;
        if until <= now {
            return Err(ArtifactError::Expired);
        }
        let url = if r.verify_only {
            String::new()
        } else {
            remote.sign_published_get(&key, &provider, until).await?
        };
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        if std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ArtifactError::Unavailable)?
            .as_secs()
            >= until
        {
            return Err(ArtifactError::Expired);
        }
        Ok(worker::InputDownloadReply {
            protocol_version: 1,
            session_id: s.session_id.clone(),
            command_id: r.command_id,
            attempt_id: lease.attempt_id.clone(),
            artifact: Some(input),
            expires_unix: cutoff,
            get_url: url,
            verify_only: r.verify_only,
        })
    }
}
