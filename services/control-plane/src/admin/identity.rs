use super::*;
use openidconnect::{
    AccessTokenHash, AsyncHttpClient, AuthenticationContextClass, AuthorizationCode, ClientId,
    ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
    url::Url,
};
use sqlx::{PgPool, Row};
use std::{sync::Arc, time::Duration};

type ConfiguredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
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
    client: ConfiguredClient,
    http: reqwest::Client,
    issuer_origin: String,
}
pub struct LoginStart {
    pub authorization_url: String,
    pub browser_secret: String,
}

fn checked_url(value: &str, development: bool) -> Result<Url, AdminError> {
    let url = Url::parse(value).map_err(|_| AdminError::Invalid)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !(url.scheme() == "https" || (development && local && url.scheme() == "http"))
    {
        return Err(AdminError::Invalid);
    }
    Ok(url)
}
async fn send(
    http: reqwest::Client,
    origin: String,
    request: openidconnect::HttpRequest,
) -> Result<openidconnect::HttpResponse, AdminError> {
    let url = Url::parse(&request.uri().to_string()).map_err(|_| AdminError::Invalid)?;
    if url.origin().ascii_serialization() != origin {
        return Err(AdminError::Forbidden);
    }
    let mut response = http
        .execute(request.try_into().map_err(|_| AdminError::Invalid)?)
        .await
        .map_err(|_| AdminError::Unavailable)?;
    const LIMIT: usize = 1024 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(AdminError::Unavailable);
    }
    let mut result = axum::http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        result = result.header(name, value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AdminError::Unavailable)?
    {
        if chunk.len() > LIMIT - body.len() {
            return Err(AdminError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    result.body(body).map_err(|_| AdminError::Unavailable)
}
struct ScopedHttp {
    client: reqwest::Client,
    origin: String,
}
impl<'c> AsyncHttpClient<'c> for ScopedHttp {
    type Error = AdminError;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<openidconnect::HttpResponse, AdminError>>
                + Send
                + Sync
                + 'c,
        >,
    >;
    fn call(&'c self, request: openidconnect::HttpRequest) -> Self::Future {
        Box::pin(send(self.client.clone(), self.origin.clone(), request))
    }
}
impl AdminIdentity {
    pub async fn discover(pool: PgPool, config: OidcConfig) -> Result<Self, AdminError> {
        if !matches!(
            config.environment.as_str(),
            "dev" | "staging" | "production"
        ) || config.required_acr.is_empty()
            || config.client_secret.len() < 32
        {
            return Err(AdminError::Invalid);
        }
        let issuer = checked_url(&config.issuer, config.environment == "dev")?;
        let admin = checked_url(&config.admin_origin, config.environment == "dev")?;
        if admin.scheme() != "https"
            || admin.path() != "/"
            || config.admin_origin.trim_end_matches('/') != admin.origin().ascii_serialization()
        {
            return Err(AdminError::Invalid);
        }
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| AdminError::Unavailable)?;
        let issuer_origin = issuer.origin().ascii_serialization();
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(config.issuer.clone()).map_err(|_| AdminError::Invalid)?,
            &ScopedHttp {
                client: http.clone(),
                origin: issuer_origin.clone(),
            },
        )
        .await
        .map_err(|_| AdminError::Unavailable)?;
        for endpoint in [
            Some(metadata.authorization_endpoint().url()),
            metadata.token_endpoint().map(|e| e.url()),
        ] {
            if !endpoint.is_some_and(|u| u.origin().ascii_serialization() == issuer_origin) {
                return Err(AdminError::Invalid);
            }
        }
        let redirect = format!(
            "{}/admin/v1/auth/callback",
            config.admin_origin.trim_end_matches('/')
        );
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(config.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(redirect).map_err(|_| AdminError::Invalid)?);
        Ok(Self {
            pool,
            config: Arc::new(config),
            client,
            http,
            issuer_origin,
        })
    }
    pub async fn start(&self, existing: Option<&CurrentSession>) -> Result<LoginStart, AdminError> {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let auth = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge)
            .set_max_age(Duration::ZERO)
            .add_auth_context_value(AuthenticationContextClass::new(
                self.config.required_acr.clone(),
            ));
        let (url, state, nonce) = auth.url();
        let browser = random_secret();
        sqlx::query("INSERT INTO admin_auth_flows(state_hash,browser_hash,nonce,pkce_verifier,expires_at,existing_session_hash,expected_principal) VALUES($1,$2,$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+300,$5,$6)")
            .bind(hash_secret(state.secret())).bind(hash_secret(&browser)).bind(nonce.secret()).bind(verifier.secret())
            .bind(existing.map(|s|&s.token_hash)).bind(existing.map(|s|&s.view.principal_id)).execute(&self.pool).await?;
        Ok(LoginStart {
            authorization_url: url.to_string(),
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
        let nonce = Nonce::new(flow.try_get("nonce")?);
        let verifier = PkceCodeVerifier::new(flow.try_get("pkce_verifier")?);
        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| AdminError::Unavailable)?
            .set_pkce_verifier(verifier)
            .request_async(&ScopedHttp {
                client: self.http.clone(),
                origin: self.issuer_origin.clone(),
            })
            .await
            .map_err(|_| AdminError::Unauthenticated)?;
        let id = response.id_token().ok_or(AdminError::Unauthenticated)?;
        let token_verifier = self.client.id_token_verifier();
        let claims = id
            .claims(&token_verifier, &nonce)
            .map_err(|_| AdminError::Unauthenticated)?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                response.access_token(),
                id.signing_alg().map_err(|_| AdminError::Unauthenticated)?,
                id.signing_key(&token_verifier)
                    .map_err(|_| AdminError::Unauthenticated)?,
            )
            .map_err(|_| AdminError::Unauthenticated)?;
            if &actual != expected {
                return Err(AdminError::Unauthenticated);
            }
        }
        if claims.auth_context_ref().map(|s| s.as_str()) != Some(self.config.required_acr.as_str())
        {
            return Err(AdminError::StepUp);
        }
        let auth_time = claims.auth_time().ok_or(AdminError::StepUp)?.timestamp();
        let token_expires = claims.expiration().timestamp();
        let mut tx = self.pool.begin().await?;
        let now: i64 =
            sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
                .fetch_one(&mut *tx)
                .await?;
        if auth_time > now + 30 || auth_time < now - 900 || token_expires <= now {
            return Err(AdminError::StepUp);
        }
        let principal=sqlx::query("SELECT principal_id FROM admin_principals WHERE issuer=$1 AND subject=$2 AND active FOR SHARE")
            .bind(&self.config.issuer).bind(claims.subject().as_str()).fetch_optional(&mut *tx).await?.ok_or(AdminError::Forbidden)?;
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
        let origin = headers
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .ok_or(AdminError::Csrf)?;
        if origin != self.config.admin_origin.trim_end_matches('/') {
            return Err(AdminError::Csrf);
        }
        let csrf = headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .ok_or(AdminError::Csrf)?;
        if hash_secret(csrf) != hash_secret(&session.view.csrf_token) {
            return Err(AdminError::Csrf);
        }
        let key = headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .ok_or(AdminError::Invalid)?;
        if !orange_protocol::valid_id(key) {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
    pub async fn revoke(
        &self,
        headers: &HeaderMap,
        session: &CurrentSession,
    ) -> Result<(), AdminError> {
        self.check_write(headers, session)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE admin_sessions SET revoked=TRUE WHERE token_hash=$1")
            .bind(&session.token_hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,occurred_at) VALUES($1,$2,'session.revoked',$3)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&session.view.principal_id).bind(session.now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
