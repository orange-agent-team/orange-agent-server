#[path = "accounts.rs"]
mod accounts;
#[path = "cloud_approvals.rs"]
mod cloud_approvals;
#[path = "cloud_runs.rs"]
mod cloud_runs;
#[path = "personal_keys.rs"]
mod personal_keys;
#[path = "team_budgets.rs"]
mod team_budgets;
#[path = "team_management.rs"]
mod team_management;
pub async fn reconcile_cloud_models(pool: &sqlx::PgPool) -> Result<usize, sqlx::Error> {
    let mut count = 0;
    while count < 32 && crate::cloud_model::reconcile_one(pool).await? {
        count += 1;
    }
    Ok(count)
}
pub async fn expire_cloud_workspaces(pool: &sqlx::PgPool) -> Result<usize, sqlx::Error> {
    let mut count = 0;
    while count < 100 && cloud_runs::expire_one(pool).await? {
        count += 1;
    }
    Ok(count)
}
#[path = "connector_access.rs"]
mod connector_access;
#[path = "model_connectors.rs"]
mod model_connectors;
#[path = "pg_connectors.rs"]
mod pg_connectors;
pub(crate) fn hosted_connector_router(
    i: UserIdentity,
    s: crate::artifacts::ArtifactStore,
) -> Router {
    model_connectors::router(i, s)
}
#[path = "confluence_connector.rs"]
mod confluence_connector;
pub fn confluence_router(i: UserIdentity, s: Option<crate::artifacts::ArtifactStore>) -> Router {
    confluence_connector::router(i, s)
}
#[path = "external_grants.rs"]
mod external_grants;
#[path = "notion_connector.rs"]
mod notion_connector;
#[path = "repository_connectors.rs"]
mod repository_connectors;
#[path = "s3_connectors.rs"]
mod s3_connectors;
#[path = "support.rs"]
mod support;
use super::{
    users::{USER_COOKIE, USER_FLOW_COOKIE, UserIdentity},
    *,
};
use crate::team::{TeamError, TeamStore};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use orange_protocol::{
    ApiError,
    team::CreateOrganization,
    user::{UserOrganization, UserOrganizationList, UserSessionView},
    valid_id,
};
use serde::Deserialize;
use std::sync::Arc;
#[path = "worker_enrollments.rs"]
mod worker_enrollments;
pub async fn prune_worker_enrollment_grants(pool: &sqlx::PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("WITH due AS (SELECT request_id FROM worker_enrollment_grants WHERE expires_at<=clock_timestamp()-interval '30 days'
        ORDER BY expires_at,request_id LIMIT 128 FOR UPDATE SKIP LOCKED) DELETE FROM worker_enrollment_grants g USING due WHERE g.request_id=due.request_id")
        .execute(pool).await?.rows_affected())
}
pub fn router(identity: UserIdentity) -> Router {
    Router::new()
        .route(
            "/api/v1/owners/{owner}/worker-enrollments/{request}",
            get(worker_enrollments::inspect)
                .put(worker_enrollments::authorize)
                .delete(worker_enrollments::revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/notion",
            get(notion_connector::inventory).post(notion_connector::grant),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/notion/{grant}",
            axum::routing::delete(notion_connector::revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/notion/{grant}/actions",
            axum::routing::post(notion_connector::perform),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/{provider}",
            get(repository_connectors::list).post(repository_connectors::grant),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/{provider}/{grant}",
            axum::routing::delete(repository_connectors::revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/{provider}/{grant}/actions",
            axum::routing::post(repository_connectors::perform),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/{provider}/{grant}/artifacts/{artifact}/content",
            axum::routing::post(repository_connectors::download),
        )
        .route(
            "/api/v1/agent/connector-tools",
            get(model_connectors::catalog),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/s3",
            get(s3_connectors::list).post(s3_connectors::grant),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/s3/{grant}",
            axum::routing::delete(s3_connectors::revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/s3/{grant}/actions",
            axum::routing::post(s3_connectors::perform),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/s3/{grant}/artifacts/{artifact}/content",
            axum::routing::post(s3_connectors::download),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/postgres",
            get(pg_connectors::list).post(pg_connectors::grant),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/postgres/{grant}",
            axum::routing::delete(pg_connectors::revoke),
        )
        .route(
            "/api/v1/agent/runs/{run}/connectors/postgres/{grant}/actions",
            axum::routing::post(pg_connectors::perform),
        )
        .route(
            "/api/v1/agent/workspace-capacity",
            get(cloud_runs::capacity),
        )
        .route(
            "/api/v1/agent/invocations/{id}",
            get(cloud_runs::lookup).delete(cloud_runs::withdraw),
        )
        .route(
            "/api/v1/agent/runs",
            get(cloud_runs::list).post(cloud_runs::create),
        )
        .route(
            "/api/v1/agent/runs/{id}",
            get(cloud_runs::get_one).delete(cloud_runs::cancel),
        )
        .route("/api/v1/agent/workspace-tools", get(cloud_runs::tools))
        .route(
            "/api/v1/agent/runs/{run}/operations",
            get(cloud_approvals::list),
        )
        .route(
            "/api/v1/agent/runs/{run}/operations/{operation}/decision",
            axum::routing::post(cloud_approvals::decide),
        )
        .route(
            "/api/v1/agent/runs/{run}/capabilities/list-directory",
            axum::routing::delete(cloud_approvals::revoke),
        )
        .route("/api/v1/auth/start", get(start))
        .route("/api/v1/auth/callback", get(callback))
        .route("/api/v1/session", get(session).delete(logout))
        .route("/api/v1/session/revocation", get(revocation_context))
        .route("/api/v1/account/support-resources", get(support::resources))
        .route(
            "/api/v1/account/support-authorizations",
            get(support::list).post(support::create),
        )
        .route(
            "/api/v1/account/support-authorizations/{id}",
            get(support::get_one).delete(support::revoke),
        )
        .route(
            "/api/v1/organizations/{id}/members",
            get(team_management::members).post(team_management::change),
        )
        .route(
            "/api/v1/organizations/{id}/invitations",
            get(team_management::outgoing).post(team_management::issue),
        )
        .route(
            "/api/v1/organizations/{id}/invitations/{invitation}",
            axum::routing::delete(team_management::revoke),
        )
        .route(
            "/api/v1/me/team-invitations",
            get(team_management::incoming),
        )
        .route(
            "/api/v1/team-invitations/{invitation}/accept",
            axum::routing::post(team_management::accept),
        )
        .route(
            "/api/v1/team-invitations/{invitation}/decline",
            axum::routing::post(team_management::decline),
        )
        .route(
            "/api/v1/organizations/{id}/budget-policy",
            get(team_budgets::policy).post(team_budgets::set_policy),
        )
        .route(
            "/api/v1/organizations/{id}/budgets",
            get(team_budgets::list).post(team_budgets::allocate),
        )
        .route(
            "/api/v1/organizations/{id}/budgets/{budget}",
            get(team_budgets::read).post(team_budgets::adjust),
        )
        .route(
            "/api/v1/account/api-keys/{previous}/replacements/{key}",
            axum::routing::put(personal_keys::replace),
        )
        .route("/api/v1/account/api-keys", get(personal_keys::list))
        .route(
            "/api/v1/account/api-keys/{key}",
            axum::routing::put(personal_keys::register).delete(personal_keys::revoke),
        )
        .route(
            "/api/v1/automation/account/wallet",
            get(personal_keys::wallet),
        )
        .route(
            "/api/v1/automation/account/budgets",
            get(personal_keys::budgets),
        )
        .route("/api/v1/account/wallet", get(accounts::wallet))
        .route("/api/v1/account/earnings", get(accounts::earnings))
        .route(
            "/api/v1/account/budgets",
            get(accounts::budgets).post(accounts::create_budget),
        )
        .route("/api/v1/account/budgets/{root}", get(accounts::budget))
        .route(
            "/api/v1/organizations",
            get(organizations).post(create_organization),
        )
        .route("/api/v1/organizations/{id}", get(organization))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(private_headers))
        .with_state(Arc::new(identity))
}
async fn private_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}
pub struct UserApiError(StatusCode, &'static str);
impl From<AuthError> for UserApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unauthenticated => Self(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            AuthError::Forbidden => Self(StatusCode::FORBIDDEN, "FORBIDDEN"),
            AuthError::Csrf => Self(StatusCode::FORBIDDEN, "CSRF_INVALID"),
            AuthError::Context => Self(StatusCode::FORBIDDEN, "AUTH_CONTEXT_REQUIRED"),
            AuthError::Invalid => Self(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
            AuthError::Unavailable => Self(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
        }
    }
}
impl From<sqlx::Error> for UserApiError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("OA041"))
        {
            return Self(StatusCode::CONFLICT, "TEAM_BUDGET_EXCEEDED");
        }
        AuthError::from(error).into()
    }
}
impl From<TeamError> for UserApiError {
    fn from(error: TeamError) -> Self {
        match error {
            TeamError::Invalid => AuthError::Invalid.into(),
            TeamError::Forbidden => AuthError::Forbidden.into(),
            TeamError::NotFound => Self(StatusCode::NOT_FOUND, "NOT_FOUND"),
            TeamError::Conflict => Self(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"),
            TeamError::Version => Self(StatusCode::CONFLICT, "VERSION_CONFLICT"),
            TeamError::Capacity => Self(StatusCode::CONFLICT, "CAPACITY_EXCEEDED"),
            TeamError::LastOwner => Self(StatusCode::CONFLICT, "LAST_OWNER_REQUIRED"),
            TeamError::Database(_) => AuthError::Unavailable.into(),
        }
    }
}
impl IntoResponse for UserApiError {
    fn into_response(self) -> Response {
        let retry = if self.0 == StatusCode::UNAUTHORIZED {
            "after_reauth"
        } else if self.0 == StatusCode::SERVICE_UNAVAILABLE {
            "query_original"
        } else {
            "never"
        };
        crate::diagnostics::typed_failure(
            self.0,
            ApiError {
                code: self.1.into(),
                message: self.1.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                retry_class: retry.into(),
            },
        )
    }
}
async fn not_found() -> UserApiError {
    UserApiError(StatusCode::NOT_FOUND, "NOT_FOUND")
}
async fn start(State(identity): State<Arc<UserIdentity>>) -> Result<Response, UserApiError> {
    let flow = identity.start().await?;
    let mut response = Redirect::to(&flow.authorization_url).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(USER_FLOW_COOKIE, &flow.browser_secret, 300)?,
    );
    Ok(response)
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
    iss: Option<String>,
}
async fn callback(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<Callback>, QueryRejection>,
) -> Result<Response, UserApiError> {
    let Query(query) = query.map_err(|_| AuthError::Invalid)?;
    if query.error.is_some()
        || query
            .iss
            .as_deref()
            .is_some_and(|issuer| issuer != identity.config.issuer)
    {
        return Err(AuthError::Unauthenticated.into());
    }
    let browser = cookie(&headers, USER_FLOW_COOKIE)?;
    let token = identity
        .callback(
            &query.state,
            query.code.as_deref().ok_or(AuthError::Invalid)?,
            &browser,
        )
        .await?;
    let mut response = Redirect::to("/organizations").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(USER_COOKIE, &token, 8 * 3600)?,
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(USER_FLOW_COOKIE, "", 0)?);
    Ok(response)
}
async fn session(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<UserSessionView>, UserApiError> {
    Ok(Json(identity.authenticate(&headers).await?.finish().await?))
}
async fn revocation_context(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<orange_protocol::user::SessionRevocationContext>, UserApiError> {
    Ok(Json(identity.revocation_context(&headers).await?))
}
async fn logout(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Response, UserApiError> {
    identity.revoke_self(&headers).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(USER_COOKIE, "", 0)?);
    Ok(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationQuery {
    after: Option<String>,
    limit: Option<u16>,
}
async fn organizations(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<OrganizationQuery>, QueryRejection>,
) -> Result<Json<UserOrganizationList>, UserApiError> {
    let Query(query) = query.map_err(|_| AuthError::Invalid)?;
    let mut guard = identity.authenticate(&headers).await?;
    let actor = guard.view().user_id.clone();
    let result = TeamStore::list_organizations_in(
        guard.transaction(),
        &actor,
        query.after.as_deref().unwrap_or(""),
        query.limit.unwrap_or(50),
    )
    .await?;
    guard.finish().await?;
    Ok(Json(result))
}
async fn organization(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<UserOrganization>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let actor = guard.view().user_id.clone();
    let result = TeamStore::organization_in(guard.transaction(), &actor, &id).await?;
    guard.finish().await?;
    Ok(Json(result))
}
async fn create_organization(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    body: Result<Json<CreateOrganization>, JsonRejection>,
) -> Result<Json<UserOrganization>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    let key = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .filter(|s| valid_id(s))
        .ok_or(AuthError::Invalid)?;
    let Json(request) = body.map_err(|_| AuthError::Invalid)?;
    let actor = guard.view().user_id.clone();
    let created =
        TeamStore::create_organization_in(guard.transaction(), &actor, key, &request).await?;
    let current =
        TeamStore::organization_in(guard.transaction(), &actor, &created.organization_id).await?;
    guard.finish().await?;
    Ok(Json(current))
}
