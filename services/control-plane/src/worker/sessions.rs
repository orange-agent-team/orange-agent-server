use orange_protocol::{valid_digest, valid_id};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("invalid control message")]
    Invalid,
    #[error("device credential or owner is unavailable")]
    Unauthorized,
    #[error("connection authority is stale or expired")]
    Stale,
    #[error("database authority unavailable")]
    Database(#[from] sqlx::Error),
}
#[derive(Debug, Clone)]
pub struct DeviceSession {
    pub node_id: String,
    pub gateway_instance_id: String,
    pub connection_epoch: i64,
    pub session_id: String,
    pub certificate_sha256: String,
    pub expires_at_unix: i64,
    pub node_control_version: u32,
    pub execution_control_version: u32,
    pub node_policy_generation: u64,
}
#[derive(Clone)]
pub struct WorkerSessionStore {
    pub(super) pool: PgPool,
    gateway: String,
}
impl WorkerSessionStore {
    pub fn new(pool: PgPool, gateway: String) -> Result<Self, SessionError> {
        if !valid_id(&gateway) {
            return Err(SessionError::Invalid);
        }
        Ok(Self { pool, gateway })
    }
    /// Fingerprint comes only from tonic's verified TLS connection metadata.
    pub(crate) async fn claim(
        &self,
        node: &str,
        fingerprint: &str,
        capabilities: &str,
        node_control_version: u32,
        execution_control_version: u32,
    ) -> Result<DeviceSession, SessionError> {
        if !valid_id(node)
            || !valid_digest(fingerprint)
            || !valid_digest(capabilities)
            || node_control_version > 2
            || execution_control_version > 2
        {
            return Err(SessionError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, node, fingerprint).await?;
        let id = Uuid::new_v4().to_string();
        let row=sqlx::query("INSERT INTO worker_sessions(node_id,gateway_instance_id,connection_epoch,session_id,certificate_sha256,capabilities_digest,last_seen_at,expires_at,node_control_version,execution_control_version) VALUES($1,$2,1,$3,$4,$5,clock_timestamp(),clock_timestamp()+interval '45 seconds',$6,$7) ON CONFLICT(node_id) DO UPDATE SET gateway_instance_id=excluded.gateway_instance_id,connection_epoch=worker_sessions.connection_epoch+1,session_id=excluded.session_id,certificate_sha256=excluded.certificate_sha256,capabilities_digest=excluded.capabilities_digest,reported_capability_snapshot_id=NULL,last_seen_at=excluded.last_seen_at,expires_at=excluded.expires_at,node_control_version=excluded.node_control_version,execution_control_version=excluded.execution_control_version RETURNING connection_epoch,floor(extract(epoch FROM expires_at))::bigint AS expires")
            .bind(node).bind(&self.gateway).bind(&id).bind(fingerprint).bind(capabilities).bind(node_control_version as i32).bind(execution_control_version as i32).fetch_one(&mut *tx).await?;
        let policy_generation: i64 =
            sqlx::query_scalar("SELECT policy_generation FROM nodes WHERE node_id=$1")
                .bind(node)
                .fetch_one(&mut *tx)
                .await?;
        let session = DeviceSession {
            node_id: node.into(),
            gateway_instance_id: self.gateway.clone(),
            connection_epoch: row.try_get("connection_epoch")?,
            session_id: id,
            certificate_sha256: fingerprint.into(),
            expires_at_unix: row.try_get("expires")?,
            node_control_version,
            execution_control_version,
            node_policy_generation: policy_generation as u64,
        };
        sqlx::query("UPDATE nodes SET state=CASE WHEN state='offline' THEN 'online' ELSE state END,version=version+1 WHERE node_id=$1").bind(node).execute(&mut *tx).await?;
        audit(&mut tx, &session, "session.claimed").await?;
        finish(tx, &session).await?;
        Ok(session)
    }
    pub(crate) async fn check(&self, session: &DeviceSession) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, &session.node_id, &session.certificate_sha256).await?;
        current(&mut tx, session).await?;
        finish(tx, session).await
    }
    pub(super) async fn authorize_in(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &DeviceSession,
    ) -> Result<(), SessionError> {
        if session.gateway_instance_id != self.gateway {
            return Err(SessionError::Stale);
        }
        authorize(tx, &session.node_id, &session.certificate_sha256).await?;
        current(tx, session).await
    }
    pub(crate) async fn heartbeat(
        &self,
        session: &DeviceSession,
        node: &str,
        epoch: u64,
        snapshot: &str,
    ) -> Result<DeviceSession, SessionError> {
        if node != session.node_id
            || i64::try_from(epoch).ok() != Some(session.connection_epoch)
            || !valid_id(snapshot)
        {
            return Err(SessionError::Stale);
        }
        let mut tx = self.pool.begin().await?;
        authorize(&mut tx, node, &session.certificate_sha256).await?;
        current(&mut tx, session).await?;
        let expires:i64=sqlx::query_scalar("UPDATE worker_sessions SET last_seen_at=clock_timestamp(),expires_at=clock_timestamp()+interval '45 seconds',reported_capability_snapshot_id=$2 WHERE node_id=$1 RETURNING floor(extract(epoch FROM expires_at))::bigint").bind(node).bind(snapshot).fetch_one(&mut *tx).await?;
        let mut renewed = session.clone();
        renewed.expires_at_unix = expires;
        let policy_generation: i64 =
            sqlx::query_scalar("SELECT policy_generation FROM nodes WHERE node_id=$1")
                .bind(node)
                .fetch_one(&mut *tx)
                .await?;
        renewed.node_policy_generation = policy_generation as u64;
        finish(tx, &renewed).await?;
        Ok(renewed)
    }
    /// Closing an old stream cannot close a newer Gateway's session.
    pub(crate) async fn disconnect(&self, session: &DeviceSession) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT node_id FROM nodes WHERE node_id=$1 FOR UPDATE")
            .bind(&session.node_id)
            .fetch_optional(&mut *tx)
            .await?;
        let affected=sqlx::query("UPDATE worker_sessions SET expires_at=LEAST(expires_at,clock_timestamp()) WHERE node_id=$1 AND gateway_instance_id=$2 AND connection_epoch=$3 AND session_id=$4 AND certificate_sha256=$5").bind(&session.node_id).bind(&session.gateway_instance_id).bind(session.connection_epoch).bind(&session.session_id).bind(&session.certificate_sha256).execute(&mut *tx).await?.rows_affected();
        if affected == 1 {
            sqlx::query("UPDATE nodes SET state='offline',version=version+1 WHERE node_id=$1 AND state='online'").bind(&session.node_id).execute(&mut *tx).await?;
            audit(&mut tx, session, "session.disconnected").await?;
        }
        super::commands::abandon_dispatched(&mut tx, session).await?;
        super::execution::abandon_session(&mut tx, session).await?;
        tx.commit().await?;
        Ok(())
    }
}
async fn authorize(
    tx: &mut Transaction<'_, Postgres>,
    node: &str,
    fingerprint: &str,
) -> Result<(), SessionError> {
    let row=sqlx::query("SELECT n.owner_id,u.personal_owner_id,u.user_id FROM nodes n JOIN worker_credentials c ON c.node_id=n.node_id JOIN owners o ON o.owner_id=n.owner_id JOIN users u ON u.user_id=c.enrollment_user_id WHERE n.node_id=$1 AND c.certificate_sha256=$2 AND n.state!='revoked' AND c.active AND c.not_before<=clock_timestamp() AND c.expires_at>clock_timestamp() AND o.state='active' AND u.active FOR UPDATE OF n FOR SHARE OF c,o,u").bind(node).bind(fingerprint).fetch_optional(&mut **tx).await?.ok_or(SessionError::Unauthorized)?;
    let owner: String = row.try_get("owner_id")?;
    if owner != row.try_get::<String, _>("personal_owner_id")? {
        let role:Option<String>=sqlx::query_scalar("SELECT role FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active FOR SHARE").bind(&owner).bind(row.try_get::<String,_>("user_id")?).fetch_optional(&mut **tx).await?;
        if !role.is_some_and(|r| r == "owner" || r == "admin") {
            return Err(SessionError::Unauthorized);
        }
    }
    Ok(())
}
async fn current(
    tx: &mut Transaction<'_, Postgres>,
    s: &DeviceSession,
) -> Result<(), SessionError> {
    let valid:Option<i64>=sqlx::query_scalar("SELECT connection_epoch FROM worker_sessions WHERE node_id=$1 AND gateway_instance_id=$2 AND connection_epoch=$3 AND session_id=$4 AND certificate_sha256=$5 AND node_control_version=$6 AND execution_control_version=$7 AND expires_at>clock_timestamp() FOR UPDATE").bind(&s.node_id).bind(&s.gateway_instance_id).bind(s.connection_epoch).bind(&s.session_id).bind(&s.certificate_sha256).bind(s.node_control_version as i32).bind(s.execution_control_version as i32).fetch_optional(&mut **tx).await?;
    if valid.is_none() {
        return Err(SessionError::Stale);
    }
    Ok(())
}
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    s: &DeviceSession,
    kind: &str,
) -> Result<(), SessionError> {
    sqlx::query("INSERT INTO worker_session_audit(event_id,node_id,gateway_instance_id,connection_epoch,session_id,kind) VALUES($1,$2,$3,$4,$5,$6)").bind(Uuid::new_v4().to_string()).bind(&s.node_id).bind(&s.gateway_instance_id).bind(s.connection_epoch).bind(&s.session_id).bind(kind).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn finish(
    mut tx: Transaction<'_, Postgres>,
    s: &DeviceSession,
) -> Result<(), SessionError> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_credentials c JOIN worker_sessions s ON s.certificate_sha256=c.certificate_sha256 WHERE c.certificate_sha256=$1 AND c.active AND c.expires_at>clock_timestamp() AND c.not_before<=clock_timestamp() AND s.node_id=$2 AND s.session_id=$3 AND s.connection_epoch=$4 AND s.expires_at>clock_timestamp())").bind(&s.certificate_sha256).bind(&s.node_id).bind(&s.session_id).bind(s.connection_epoch).fetch_one(&mut *tx).await?;
    if !valid {
        return Err(SessionError::Stale);
    }
    tx.commit().await?;
    Ok(())
}
