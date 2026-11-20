use super::{
    oidc::{OidcProvider, ProviderConfig},
    *,
};
use orange_protocol::user::UserSessionView;
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
pub const USER_COOKIE: &str = "__Host-orange-user";
pub const USER_FLOW_COOKIE: &str = "__Host-orange-user-flow";
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserOidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub user_origin: String,
    pub required_acr: String,
    pub environment: String,
}
#[derive(Clone)]
pub struct UserIdentity {
    pub pool: PgPool,
    pub config: Arc<UserOidcConfig>,
    provider: OidcProvider,
    pub notion_sources: crate::notion::registry::Sources,
    pub repository_sources: crate::repo_connector::registry::Sources,
    pub s3_sources: crate::s3_sources::Sources,
    pub pg_sources: crate::pg_view_sources::Sources,
}
pub struct UserLoginStart {
    pub authorization_url: String,
    pub browser_secret: String,
}
/// Holds current session/user authority through the same transaction as the domain operation.
pub struct UserSessionGuard {
    transaction: Transaction<'static, Postgres>,
    view: UserSessionView,
}
impl UserSessionGuard {
    pub fn view(&self) -> &UserSessionView {
        &self.view
    }
    pub(crate) fn transaction(&mut self) -> &mut Transaction<'static, Postgres> {
        &mut self.transaction
    }
    pub async fn finish(mut self) -> Result<UserSessionView, AuthError> {
        let live:bool=sqlx::query_scalar("SELECT NOT revoked AND idle_expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint AND absolute_expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint FROM user_sessions WHERE session_id=$1")
            .bind(&self.view.session_id).fetch_one(&mut *self.transaction).await?;
        if !live {
            return Err(AuthError::Unauthenticated);
        }
        self.transaction.commit().await?;
        Ok(self.view)
    }
}
impl UserIdentity {
    pub async fn revocation_context(
        &self,
        headers: &HeaderMap,
    ) -> Result<orange_protocol::user::SessionRevocationContext, AuthError> {
        let hash = hash_secret(&cookie(headers, USER_COOKIE)?);
        let csrf: Option<String> = sqlx::query_scalar(
            "SELECT csrf_token FROM user_sessions WHERE token_hash=$1 AND NOT revoked",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(orange_protocol::user::SessionRevocationContext {
            csrf_token: csrf.ok_or(AuthError::Unauthenticated)?,
        })
    }
    pub async fn discover(pool: PgPool, config: UserOidcConfig) -> Result<Self, AuthError> {
        if !matches!(
            config.environment.as_str(),
            "dev" | "staging" | "production"
        ) || config.required_acr.is_empty()
        {
            return Err(AuthError::Invalid);
        }
        let provider = OidcProvider::discover(ProviderConfig {
            issuer: config.issuer.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            origin: config.user_origin.clone(),
            callback_path: "/api/v1/auth/callback".into(),
            development: config.environment == "dev",
        })
        .await?;
        Ok(Self {
            pool,
            config: Arc::new(config),
            pg_sources: Default::default(),
            s3_sources: Default::default(),
            repository_sources: Default::default(),
            notion_sources: Default::default(),
            provider,
        })
    }
    pub async fn start(&self) -> Result<UserLoginStart, AuthError> {
        let auth = self.provider.start(&self.config.required_acr)?;
        let browser = random_secret();
        sqlx::query("INSERT INTO user_auth_flows(state_hash,browser_hash,nonce,pkce_verifier,expires_at) VALUES($1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+300)")
            .bind(hash_secret(&auth.state)).bind(hash_secret(&browser)).bind(auth.nonce).bind(auth.verifier).execute(&self.pool).await?;
        Ok(UserLoginStart {
            authorization_url: auth.authorization_url,
            browser_secret: browser,
        })
    }
    pub async fn callback(
        &self,
        state: &str,
        code: &str,
        browser: &str,
    ) -> Result<String, AuthError> {
        if state.len() > 256 || code.is_empty() || code.len() > 4096 {
            return Err(AuthError::Invalid);
        }
        let flow=sqlx::query("DELETE FROM user_auth_flows WHERE state_hash=$1 AND browser_hash=$2 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint RETURNING nonce,pkce_verifier")
            .bind(hash_secret(state)).bind(hash_secret(browser)).fetch_optional(&self.pool).await?.ok_or(AuthError::Unauthenticated)?;
        let claims = self
            .provider
            .exchange(code, flow.try_get("pkce_verifier")?, flow.try_get("nonce")?)
            .await?;
        if claims.acr() != Some(self.config.required_acr.as_str()) {
            return Err(AuthError::Context);
        }
        let auth_time = claims.auth_time().ok_or(AuthError::Context)?;
        let mut tx = self.pool.begin().await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *tx)
                .await?;
        if auth_time < now - 900 || auth_time > now + 30 || claims.expires_at() <= now {
            return Err(AuthError::Context);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "orange.user.identity:{}:{}",
                claims.issuer(),
                claims.subject()
            ))
            .execute(&mut *tx)
            .await?;
        let user=sqlx::query("SELECT u.user_id,u.active,w.state FROM users u JOIN owners w ON w.owner_id=u.personal_owner_id WHERE u.issuer=$1 AND u.subject=$2 FOR UPDATE OF u FOR SHARE OF w")
            .bind(claims.issuer()).bind(claims.subject()).fetch_optional(&mut *tx).await?;
        let user_id = if let Some(user) = user {
            if !user.try_get::<bool, _>("active")?
                || user.try_get::<String, _>("state")? != "active"
            {
                return Err(AuthError::Forbidden);
            }
            user.try_get::<String, _>("user_id")?
        } else {
            let id = Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,$2,$3,$1)",
            )
            .bind(&id)
            .bind(claims.issuer())
            .bind(claims.subject())
            .execute(&mut *tx)
            .await?;
            id
        };
        let token = random_secret();
        let session = Uuid::new_v4().to_string();
        let absolute = claims.expires_at().min(now + 8 * 3600);
        sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(hash_secret(&token)).bind(&session).bind(&user_id).bind(random_secret()).bind(auth_time).bind((now+900).min(absolute)).bind(absolute).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO user_auth_audit(event_id,user_id,session_id,event_type,occurred_at) VALUES($1,$2,$3,'session.established',$4)").bind(Uuid::new_v4().to_string()).bind(user_id).bind(session).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(token)
    }
    /// Resolves HTTP identity and CSRF before creating a nonserializable storage
    /// actor. Storage rechecks this session and current ACL after every object I/O.
    pub async fn artifact_actor(
        &self,
        headers: &HeaderMap,
        write: bool,
    ) -> Result<crate::artifacts::ArtifactActor, AuthError> {
        let guard = self.authenticate(headers).await?;
        if write {
            check_write(headers, &self.config.user_origin, &guard.view().csrf_token)?;
        }
        let actor = crate::artifacts::ArtifactActor::from_verified_session(guard.view(), write);
        guard.finish().await?;
        Ok(actor)
    }
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<UserSessionGuard, AuthError> {
        let token = hash_secret(&cookie(headers, USER_COOKIE)?);
        let mut tx = self.pool.begin().await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *tx)
                .await?;
        let row=sqlx::query("SELECT s.*,u.personal_owner_id FROM user_sessions s JOIN users u USING(user_id) JOIN owners w ON w.owner_id=u.personal_owner_id WHERE s.token_hash=$1 AND NOT s.revoked AND s.idle_expires_at>$2 AND s.absolute_expires_at>$2 AND u.active AND w.state='active' FOR UPDATE OF s FOR SHARE OF u,w")
            .bind(&token).bind(now).fetch_optional(&mut *tx).await?.ok_or(AuthError::Unauthenticated)?;
        let expires = (now + 900).min(row.try_get::<i64, _>("absolute_expires_at")?);
        sqlx::query("UPDATE user_sessions SET idle_expires_at=$2 WHERE token_hash=$1")
            .bind(token)
            .bind(expires)
            .execute(&mut *tx)
            .await?;
        let view = UserSessionView {
            session_id: row.try_get("session_id")?,
            user_id: row.try_get("user_id")?,
            personal_owner_id: row.try_get("personal_owner_id")?,
            csrf_token: row.try_get("csrf_token")?,
            expires_at: timestamp(expires)?,
        };
        Ok(UserSessionGuard {
            transaction: tx,
            view,
        })
    }
    pub async fn revoke_self(&self, headers: &HeaderMap) -> Result<(), AuthError> {
        let hash = hash_secret(&cookie(headers, USER_COOKIE)?);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM user_sessions WHERE token_hash=$1 FOR UPDATE")
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AuthError::Unauthenticated)?;
        check_write(
            headers,
            &self.config.user_origin,
            &row.try_get::<String, _>("csrf_token")?,
        )?;
        if !row.try_get::<bool, _>("revoked")? {
            sqlx::query("UPDATE user_sessions SET revoked=TRUE WHERE token_hash=$1")
                .bind(hash)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO user_auth_audit(event_id,user_id,session_id,event_type,occurred_at) VALUES($1,$2,$3,'session.revoked',FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
                .bind(Uuid::new_v4().to_string()).bind(row.try_get::<String,_>("user_id")?).bind(row.try_get::<String,_>("session_id")?).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
