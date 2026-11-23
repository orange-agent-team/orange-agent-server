//! Platform-wide signing-key withdrawal. The action engine owns MFA, approval, replay and audit.
use super::*;
use axum::extract::{Query, rejection::QueryRejection};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Withdraw {
    pub signing_key_sha256: String,
}
impl Withdraw {
    pub(super) fn validate(&self, r: &ActionRequest) -> Result<(), AdminError> {
        if r.action_type != "device_issuer.withdraw"
            || r.scope.environment != "dev"
            || r.scope.kind != "platform"
            || r.scope.scope_id != "platform"
            || !orange_protocol::valid_digest(&self.signing_key_sha256)
            || !orange_protocol::valid_digest(&r.target_version)
        {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}

async fn projection(tx: &mut Tx<'_>, key: &str) -> Result<Value, AdminError> {
    let row=sqlx::query("WITH live_sessions AS (
            SELECT s.* FROM worker_sessions s JOIN worker_credentials c USING(certificate_sha256,node_id)
            WHERE s.expires_at>statement_timestamp() AND c.active
              AND c.not_before<=statement_timestamp() AND c.expires_at>statement_timestamp()
        ) SELECT (SELECT count(*) FROM worker_issuer_public_keys WHERE spki_sha256=$1) AS roots,
            (SELECT action_id FROM worker_issuer_withdrawals WHERE signing_key_sha256=$1) AS action,
            (SELECT count(*) FROM live_sessions s WHERE EXISTS
               (SELECT FROM worker_control_trust_paths t WHERE t.node_id=s.node_id AND t.session_id=s.session_id AND t.certificate_sha256=s.certificate_sha256 AND $1=ANY(t.signing_keys))) AS live,
            (SELECT count(*) FROM live_sessions s WHERE NOT EXISTS
               (SELECT FROM worker_control_trust_paths t WHERE t.node_id=s.node_id AND t.session_id=s.session_id AND t.certificate_sha256=s.certificate_sha256)) AS unknown,
            (SELECT count(*) FROM worker_enrollment_certificates e JOIN worker_issuer_public_keys k USING(issuer_sha256)
               JOIN worker_credentials c USING(certificate_sha256) WHERE k.spki_sha256=$1 AND e.redeemed_at IS NOT NULL AND c.active AND c.expires_at>statement_timestamp()) AS credentials")
        .bind(key).fetch_one(&mut **tx).await?;
    let roots: i64 = row.try_get("roots")?;
    let prior: Option<String> = row.try_get("action")?;
    if roots == 0 && prior.is_none() {
        return Err(AdminError::NotFound);
    }
    let version = digest("admin.device-issuer-withdraw.target.v1", &(key, &prior))?;
    Ok(
        json!({"signing_key_sha256":key,"state":if prior.is_some(){"withdrawn"}else{"observed"},"target_version":version,"withdrawal_action_id":prior,
        "observed_ca_certificates":roots.to_string(),"valid_issued_credentials":row.try_get::<i64,_>("credentials")?.to_string(),
        "live_verified_sessions":row.try_get::<i64,_>("live")?.to_string(),"live_unattributed_sessions":row.try_get::<i64,_>("unknown")?.to_string()}),
    )
}
pub(super) async fn target(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Withdraw,
) -> Result<(i64, String), AdminError> {
    p.validate(r)?;
    let view = projection(tx, &p.signing_key_sha256).await?;
    if view["target_version"] != r.target_version {
        return Err(AdminError::Precondition);
    }
    if view["state"] != "observed" {
        return Err(AdminError::Conflict);
    }
    Ok((1, "observed".into()))
}
pub(super) async fn apply(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Withdraw,
    action: &str,
    maker: &str,
    checker: &str,
) -> Result<(), AdminError> {
    if maker == checker {
        return Err(AdminError::Forbidden);
    }
    sqlx::query("SELECT revision FROM worker_issuer_policy_version WHERE singleton FOR UPDATE")
        .fetch_one(&mut **tx)
        .await?;
    target(tx, r, p).await?;
    sqlx::query("INSERT INTO worker_issuer_withdrawals(signing_key_sha256,action_id,maker_id,checker_id) VALUES($1,$2,$3,$4)")
        .bind(&p.signing_key_sha256).bind(action).bind(maker).bind(checker).execute(&mut **tx).await?;
    sqlx::query("UPDATE worker_issuer_policy_version SET revision=revision+1 WHERE singleton")
        .execute(&mut **tx)
        .await?;
    // Expire active sessions first. This serializes with their existing final session checks,
    // including binaries that predate path provenance. An unrelated verified path remains usable.
    sqlx::query("UPDATE worker_sessions s SET expires_at=clock_timestamp() WHERE s.expires_at>clock_timestamp()
        AND NOT worker_control_path_allowed(s.node_id,s.session_id,s.certificate_sha256)").execute(&mut **tx).await?;
    sqlx::query("UPDATE worker_credentials c SET active=false FROM worker_enrollment_certificates e JOIN worker_issuer_public_keys k USING(issuer_sha256)
        WHERE k.spki_sha256=$1 AND c.certificate_sha256=e.certificate_sha256 AND e.redeemed_at IS NOT NULL AND c.active").bind(&p.signing_key_sha256)
        .execute(&mut **tx).await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
}
pub async fn review(
    State(identity): State<Arc<AdminIdentity>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    q: Result<Query<KeyQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    if !orange_protocol::valid_digest(&key)
        || q.environment != "dev"
        || q.scope_kind != "platform"
        || q.scope_id != "platform"
    {
        return Err(AdminError::Invalid);
    }
    let scope = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    let req = crate::admin::routes::audit::request_id(&headers)?;
    let mut tx = identity.pool.begin().await?;
    let current = identity.current_in(&headers, &mut tx).await?;
    current.authorize("device_issuers.read", &scope)?;
    let view = projection(&mut tx, &key).await?;
    crate::admin::routes::audit::record_read(
        &mut tx,
        &current,
        &scope,
        "device_issuer_key.read",
        Some(&key),
        &req,
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("device_issuers.read", &scope)?;
    tx.commit().await?;
    crate::admin::routes::audit::response(view, &req)
}
