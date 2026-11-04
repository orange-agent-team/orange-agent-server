use super::AuthError;
use openidconnect::{
    AccessTokenHash, AsyncHttpClient, AuthenticationContextClass, AuthorizationCode, ClientId,
    ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
    url::Url,
};
use std::time::Duration;
type ConfiguredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
pub struct ProviderConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub origin: String,
    pub callback_path: String,
    pub development: bool,
}
pub struct AuthorizationFlow {
    pub authorization_url: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
}
/// Not deserializable: only a successful validated code exchange can construct this value.
pub struct VerifiedIdentity {
    issuer: String,
    subject: String,
    acr: Option<String>,
    auth_time: Option<i64>,
    expires_at: i64,
}
impl VerifiedIdentity {
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    pub fn subject(&self) -> &str {
        &self.subject
    }
    pub fn acr(&self) -> Option<&str> {
        self.acr.as_deref()
    }
    pub fn auth_time(&self) -> Option<i64> {
        self.auth_time
    }
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
}
#[derive(Clone)]
pub struct OidcProvider {
    client: ConfiguredClient,
    http: ScopedHttp,
    issuer: String,
}
pub fn checked_url(value: &str, development: bool) -> Result<Url, AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::Invalid)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !(url.scheme() == "https" || (development && local && url.scheme() == "http"))
    {
        return Err(AuthError::Invalid);
    }
    Ok(url)
}
pub fn checked_origin(value: &str) -> Result<Url, AuthError> {
    let url = checked_url(value, false)?;
    if url.path() != "/" || value.trim_end_matches('/') != url.origin().ascii_serialization() {
        return Err(AuthError::Invalid);
    }
    Ok(url)
}
async fn send(
    http: reqwest::Client,
    origin: String,
    request: openidconnect::HttpRequest,
) -> Result<openidconnect::HttpResponse, AuthError> {
    let url = Url::parse(&request.uri().to_string()).map_err(|_| AuthError::Invalid)?;
    if url.origin().ascii_serialization() != origin {
        return Err(AuthError::Forbidden);
    }
    let mut response = http
        .execute(request.try_into().map_err(|_| AuthError::Invalid)?)
        .await
        .map_err(|_| AuthError::Unavailable)?;
    const LIMIT: usize = 1024 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(AuthError::Unavailable);
    }
    let mut result = axum::http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        result = result.header(name, value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Unavailable)? {
        if chunk.len() > LIMIT - body.len() {
            return Err(AuthError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    result.body(body).map_err(|_| AuthError::Unavailable)
}
#[derive(Clone)]
struct ScopedHttp {
    client: reqwest::Client,
    origin: String,
}
impl<'c> AsyncHttpClient<'c> for ScopedHttp {
    type Error = AuthError;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<openidconnect::HttpResponse, AuthError>>
                + Send
                + Sync
                + 'c,
        >,
    >;
    fn call(&'c self, request: openidconnect::HttpRequest) -> Self::Future {
        Box::pin(send(self.client.clone(), self.origin.clone(), request))
    }
}
impl OidcProvider {
    pub async fn discover(config: ProviderConfig) -> Result<Self, AuthError> {
        if config.client_id.is_empty()
            || config.client_id.len() > 256
            || config.client_secret.len() < 32
            || !config.callback_path.starts_with('/')
            || config.callback_path.starts_with("//")
            || config.callback_path.contains(['?', '#'])
        {
            return Err(AuthError::Invalid);
        }
        let issuer = checked_url(&config.issuer, config.development)?;
        checked_origin(&config.origin)?;
        let client = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| AuthError::Unavailable)?;
        let http = ScopedHttp {
            client,
            origin: issuer.origin().ascii_serialization(),
        };
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(config.issuer.clone()).map_err(|_| AuthError::Invalid)?,
            &http,
        )
        .await
        .map_err(|_| AuthError::Unavailable)?;
        for endpoint in [
            Some(metadata.authorization_endpoint().url()),
            metadata.token_endpoint().map(|e| e.url()),
        ] {
            if !endpoint.is_some_and(|url| url.origin().ascii_serialization() == http.origin) {
                return Err(AuthError::Invalid);
            }
        }
        let redirect = format!(
            "{}{}",
            config.origin.trim_end_matches('/'),
            config.callback_path
        );
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id),
            Some(ClientSecret::new(config.client_secret)),
        )
        .set_redirect_uri(RedirectUrl::new(redirect).map_err(|_| AuthError::Invalid)?);
        Ok(Self {
            client,
            http,
            issuer: config.issuer,
        })
    }
    pub fn start(&self, acr: &str) -> Result<AuthorizationFlow, AuthError> {
        if acr.is_empty() || acr.len() > 128 {
            return Err(AuthError::Invalid);
        }
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge)
            .set_max_age(Duration::ZERO)
            .add_auth_context_value(AuthenticationContextClass::new(acr.into()))
            .url();
        Ok(AuthorizationFlow {
            authorization_url: url.to_string(),
            state: state.secret().clone(),
            nonce: nonce.secret().clone(),
            verifier: verifier.secret().clone(),
        })
    }
    pub async fn exchange(
        &self,
        code: &str,
        verifier: String,
        nonce: String,
    ) -> Result<VerifiedIdentity, AuthError> {
        if code.is_empty() || code.len() > 4096 {
            return Err(AuthError::Invalid);
        }
        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code.into()))
            .map_err(|_| AuthError::Unavailable)?
            .set_pkce_verifier(PkceCodeVerifier::new(verifier))
            .request_async(&self.http)
            .await
            .map_err(|_| AuthError::Unauthenticated)?;
        let token = response.id_token().ok_or(AuthError::Unauthenticated)?;
        let verifier = self.client.id_token_verifier();
        let claims = token
            .claims(&verifier, &Nonce::new(nonce))
            .map_err(|_| AuthError::Unauthenticated)?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                response.access_token(),
                token
                    .signing_alg()
                    .map_err(|_| AuthError::Unauthenticated)?,
                token
                    .signing_key(&verifier)
                    .map_err(|_| AuthError::Unauthenticated)?,
            )
            .map_err(|_| AuthError::Unauthenticated)?;
            if &actual != expected {
                return Err(AuthError::Unauthenticated);
            }
        }
        if claims.subject().as_str().is_empty() || claims.subject().as_str().len() > 512 {
            return Err(AuthError::Unauthenticated);
        }
        Ok(VerifiedIdentity {
            issuer: self.issuer.clone(),
            subject: claims.subject().as_str().into(),
            acr: claims.auth_context_ref().map(|s| s.as_str().into()),
            auth_time: claims.auth_time().map(|t| t.timestamp()),
            expires_at: claims.expiration().timestamp(),
        })
    }
}
