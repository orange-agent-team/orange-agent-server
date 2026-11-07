use super::*;
use sqlx::Row;
impl WorkerSessionStore {
    /// Fingerprint is supplied exclusively by verified TLS transport metadata.
    pub(crate) async fn input_peer(
        &self,
        fingerprint: &str,
        session: &str,
    ) -> Result<DeviceSession, SessionError> {
        if !orange_protocol::valid_id(session) {
            return Err(SessionError::Invalid);
        }
        let row=sqlx::query("SELECT *,floor(extract(epoch FROM expires_at))::bigint AS expires FROM worker_sessions WHERE certificate_sha256=$1 AND session_id=$2 AND input_artifacts_version=1").bind(fingerprint).bind(session).fetch_optional(&self.pool).await?.ok_or(SessionError::Unauthorized)?;
        let s = DeviceSession {
            node_id: row.try_get("node_id")?,
            gateway_instance_id: row.try_get("gateway_instance_id")?,
            connection_epoch: row.try_get("connection_epoch")?,
            session_id: row.try_get("session_id")?,
            certificate_sha256: row.try_get("certificate_sha256")?,
            expires_at_unix: row.try_get("expires")?,
            node_control_version: row.try_get::<i32, _>("node_control_version")? as u32,
            execution_control_version: row.try_get::<i32, _>("execution_control_version")? as u32,
            rejection_receipts_version: row.try_get::<i32, _>("rejection_receipts_version")? as u32,
            input_artifacts_version: 1,
            renewal_version: row.try_get::<i32, _>("renewal_version")? as u32,
            node_policy_generation: 0,
        };
        let mut tx = self.pool.begin().await?;
        self.authorize_in(&mut tx, &s).await?;
        sessions::finish(tx, &s).await?;
        Ok(s)
    }
}
