use super::*;
use crate::auth::oidc::{OidcProvider, ProviderConfig};
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub admin_origin: String,
    pub required_acr: String,
    pub environment: String,
}
#[derive(Clone)]
pub struct AdminIdentity {
    pub pool: PgPool,
    pub config: Arc<OidcConfig>,
    provider: OidcProvider,
}
pub struct LoginStart {
    pub authorization_url: String,
    pub browser_secret: String,
}

impl AdminIdentity {
    pub async fn revocation_context(
        &self,
        headers: &HeaderMap,
    ) -> Result<orange_protocol::user::SessionRevocationContext, AdminError> {
        let hash = hash_secret(&cookie(headers, SESSION_COOKIE)?);
        let csrf: Option<String> = sqlx::query_scalar(
            "SELECT csrf_token FROM admin_sessions WHERE token_hash=$1 AND NOT revoked",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(orange_protocol::user::SessionRevocationContext {
            csrf_token: csrf.ok_or(AdminError::Unauthenticated)?,
        })
    }
    pub async fn revoke_self(&self, headers: &HeaderMap) -> Result<(), AdminError> {
        let hash = hash_secret(&cookie(headers, SESSION_COOKIE)?);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM admin_sessions WHERE token_hash=$1 FOR UPDATE")
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AdminError::Unauthenticated)?;
        crate::auth::check_write(
            headers,
            &self.config.admin_origin,
            &row.try_get::<String, _>("csrf_token")?,
        )?;
        if !row.try_get::<bool, _>("revoked")? {
            sqlx::query("UPDATE admin_sessions SET revoked=TRUE WHERE token_hash=$1")
                .bind(hash)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,occurred_at) VALUES($1,$2,'session.revoked',FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
                .bind(uuid::Uuid::new_v4().to_string()).bind(row.try_get::<String,_>("principal_id")?).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn discover(pool: PgPool, config: OidcConfig) -> Result<Self, AdminError> {
        if !matches!(
            config.environment.as_str(),
            "dev" | "staging" | "production"
        ) || config.required_acr.is_empty()
            || config.client_secret.len() < 32
        {
            return Err(AdminError::Invalid);
        }
        let provider = OidcProvider::discover(ProviderConfig {
            issuer: config.issuer.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            origin: config.admin_origin.clone(),
            callback_path: "/admin/v1/auth/callback".into(),
            development: config.environment == "dev",
        })
        .await?;
        Ok(Self {
            pool,
            config: Arc::new(config),
            provider,
        })
    }
    pub async fn start(&self, existing: Option<&CurrentSession>) -> Result<LoginStart, AdminError> {
        let auth = self.provider.start(&self.config.required_acr)?;
        let browser = random_secret();
        sqlx::query("INSERT INTO admin_auth_flows(state_hash,browser_hash,nonce,pkce_verifier,expires_at,existing_session_hash,expected_principal) VALUES($1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+300,$5,$6)")
            .bind(hash_secret(&auth.state)).bind(hash_secret(&browser)).bind(&auth.nonce).bind(&auth.verifier)
            .bind(existing.map(|s|&s.token_hash)).bind(existing.map(|s|&s.view.principal_id)).execute(&self.pool).await?;
        Ok(LoginStart {
            authorization_url: auth.authorization_url,
            browser_secret: browser,
        })
    }
    pub async fn callback(
        &self,
        state: &str,
        code: &str,
        browser: &str,
    ) -> Result<String, AdminError> {
        if state.len() > 256 || code.is_empty() || code.len() > 4096 {
            return Err(AdminError::Invalid);
        }
        // DELETE RETURNING is the one-time exchange barrier. Never retry an uncertain code exchange.
        let flow=sqlx::query("DELETE FROM admin_auth_flows WHERE state_hash=$1 AND browser_hash=$2 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint RETURNING *")
            .bind(hash_secret(state)).bind(hash_secret(browser)).fetch_optional(&self.pool).await?.ok_or(AdminError::Unauthenticated)?;
        let claims = self
            .provider
            .exchange(code, flow.try_get("pkce_verifier")?, flow.try_get("nonce")?)
            .await?;
        if claims.acr() != Some(self.config.required_acr.as_str()) {
            return Err(AdminError::StepUp);
        }
        let auth_time = claims.auth_time().ok_or(AdminError::StepUp)?;
        let token_expires = claims.expires_at();
        let mut tx = self.pool.begin().await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *tx)
                .await?;
        if auth_time > now + 30 || auth_time < now - 900 || token_expires <= now {
            return Err(AdminError::StepUp);
        }
        let principal=sqlx::query("SELECT principal_id FROM admin_principals WHERE issuer=$1 AND subject=$2 AND active FOR SHARE")
            .bind(&self.config.issuer).bind(claims.subject()).fetch_optional(&mut *tx).await?.ok_or(AdminError::Forbidden)?;
        let principal_id: String = principal.try_get("principal_id")?;
        let grant:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_grants WHERE principal_id=$1 AND environment=$2 AND NOT revoked AND expires_at>$3)")
            .bind(&principal_id).bind(&self.config.environment).bind(now).fetch_one(&mut *tx).await?;
        if !grant {
            return Err(AdminError::Forbidden);
        }
        let expected: Option<String> = flow.try_get("expected_principal")?;
        if expected.as_ref().is_some_and(|id| id != &principal_id) {
            return Err(AdminError::Forbidden);
        }
        if let Some(old) = flow.try_get::<Option<String>, _>("existing_session_hash")? {
            let result=sqlx::query("UPDATE admin_sessions SET revoked=TRUE WHERE token_hash=$1 AND principal_id=$2 AND NOT revoked AND idle_expires_at>$3 AND absolute_expires_at>$3")
                .bind(old).bind(&principal_id).bind(now).execute(&mut *tx).await?;
            if result.rows_affected() != 1 {
                return Err(AdminError::Unauthenticated);
            }
        }
        let token = random_secret();
        let csrf = random_secret();
        let absolute = token_expires.min(now + 8 * 3600);
        sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(hash_secret(&token)).bind(&principal_id).bind(csrf).bind(auth_time).bind((now+900).min(absolute)).bind(absolute).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,occurred_at) VALUES($1,$2,'session.established',$3)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(principal_id).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(token)
    }
    pub async fn current(&self, headers: &HeaderMap) -> Result<CurrentSession, AdminError> {
        let token_hash = hash_secret(&cookie(headers, SESSION_COOKIE)?);
        let mut tx = self.pool.begin().await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *tx)
                .await?;
        let row=sqlx::query("SELECT s.*,p.policy_version FROM admin_sessions s JOIN admin_principals p USING(principal_id) WHERE s.token_hash=$1 AND NOT s.revoked AND p.active AND s.idle_expires_at>$2 AND s.absolute_expires_at>$2 FOR UPDATE OF s FOR SHARE OF p")
            .bind(&token_hash).bind(now).fetch_optional(&mut *tx).await?.ok_or(AdminError::Unauthenticated)?;
        let principal_id: String = row.try_get("principal_id")?;
        let rows=sqlx::query("SELECT * FROM admin_grants WHERE principal_id=$1 AND environment=$2 AND NOT revoked AND expires_at>$3 FOR SHARE")
            .bind(&principal_id).bind(&self.config.environment).bind(now).fetch_all(&mut *tx).await?;
        let registry = role_permissions();
        let mut roles = BTreeSet::new();
        let mut grants: BTreeMap<Scope, BTreeSet<String>> = BTreeMap::new();
        for grant in rows {
            let role: String = grant.try_get("role")?;
            let Some(permissions) = registry.get(&role) else {
                return Err(AdminError::Forbidden);
            };
            roles.insert(role);
            let scope = Scope {
                environment: grant.try_get("environment")?,
                kind: grant.try_get("scope_kind")?,
                scope_id: grant.try_get("scope_id")?,
            };
            grants
                .entry(scope)
                .or_default()
                .extend(permissions.iter().cloned());
        }
        if grants.is_empty() {
            return Err(AdminError::Forbidden);
        }
        if grants.len() > 100 {
            return Err(AdminError::Unavailable);
        }
        // Schema0.1 has unpaired scope/permission arrays: publish only their intersection.
        // Server checks the full per-scope grant map on every resource operation.
        let mut common = grants
            .values()
            .next()
            .cloned()
            .ok_or(AdminError::Forbidden)?;
        for permissions in grants.values() {
            common = common.intersection(permissions).cloned().collect();
        }
        let absolute: i64 = row.try_get("absolute_expires_at")?;
        let expires = (now + 900).min(absolute);
        sqlx::query("UPDATE admin_sessions SET idle_expires_at=$2 WHERE token_hash=$1")
            .bind(&token_hash)
            .bind(expires)
            .execute(&mut *tx)
            .await?;
        let auth_time = row.try_get("auth_time")?;
        let view = SessionView {
            principal_id,
            roles: roles.into_iter().collect(),
            scopes: grants.keys().cloned().collect(),
            permissions: common.into_iter().collect(),
            mfa_verified: true,
            step_up_at: timestamp(auth_time)?,
            expires_at: timestamp(expires)?,
            csrf_token: row.try_get("csrf_token")?,
            policy_version: row.try_get::<i64, _>("policy_version")?.to_string(),
        };
        tx.commit().await?;
        Ok(CurrentSession {
            view,
            token_hash,
            auth_time,
            now,
            grants,
        })
    }
    pub fn check_write(
        &self,
        headers: &HeaderMap,
        session: &CurrentSession,
    ) -> Result<(), AdminError> {
        crate::auth::check_write(headers, &self.config.admin_origin, &session.view.csrf_token)
            .map_err(Into::into)
    }
    pub async fn revoke(
        &self,
        headers: &HeaderMap,
        session: &CurrentSession,
    ) -> Result<(), AdminError> {
        if hash_secret(&cookie(headers, SESSION_COOKIE)?) != session.token_hash {
            return Err(AdminError::Forbidden);
        }
        self.revoke_self(headers).await
    }
}
