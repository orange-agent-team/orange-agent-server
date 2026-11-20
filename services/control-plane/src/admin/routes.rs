use super::{identity::AdminIdentity, *};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Query, State, rejection::QueryRejection},
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::Redirect,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;

pub(super) mod audit;
mod cases;

pub fn router(identity: AdminIdentity) -> Router {
    Router::new()
        .route(
            "/admin/v1/support-authorizations/{id}",
            get(super::actions::support::review),
        )
        .route(
            "/admin/v1/support-grants/{id}",
            get(super::actions::support::detail),
        )
        .route(
            "/admin/v1/support-content",
            post(super::actions::support::read_content),
        )
        .route(
            "/admin/v1/support-grants",
            get(super::actions::support::directory),
        )
        .route("/admin/v1/auth/start", get(start))
        .route("/admin/v1/auth/callback", get(callback))
        .route("/admin/v1/auth/step-up", post(step_up))
        .route("/admin/v1/session", get(session).delete(logout))
        .route("/admin/v1/session/revocation", get(revocation_context))
        .route(
            "/admin/v1/session/permissions",
            get(audit::scope_permissions),
        )
        .route("/admin/v1/overview", get(overview))
        .route(
            "/admin/v1/price-drafts",
            get(super::actions::prices::list).post(super::actions::prices::create),
        )
        .route(
            "/admin/v1/price-drafts/{price_id}",
            get(super::actions::prices::get_one),
        )
        .route(
            "/admin/v1/reconciliation-cases/{case_id}/activity",
            get(cases::activity),
        )
        .route("/admin/v1/reconciliation-cases", get(cases::list))
        .route(
            "/admin/v1/reconciliation-cases/{case_id}",
            get(cases::detail),
        )
        .route("/admin/v1/audit-events", get(audit::list))
        .route("/admin/v1/audit-events/{event_id}", get(audit::get_one))
        .route("/admin/v1/nodes/{node_id}", get(super::actions::get_node))
        .route(
            "/admin/v1/actions",
            get(actions).post(super::actions::create),
        )
        .route(
            "/admin/v1/actions/{action_id}",
            get(super::actions::get_one),
        )
        .route(
            "/admin/v1/actions/{action_id}/approvals",
            post(super::actions::decide),
        )
        .route(
            "/admin/v1/actions/{action_id}/execute",
            post(super::actions::execute),
        )
        .route(
            "/admin/v1/actions/{action_id}/cancel",
            post(super::actions::cancel),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(Arc::new(identity))
}
async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}
async fn start(State(identity): State<Arc<AdminIdentity>>) -> Result<Response, AdminError> {
    let login = identity.start(None).await?;
    let mut response = Redirect::to(&login.authorization_url).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(FLOW_COOKIE, &login.browser_secret, 300)?,
    );
    Ok(response)
}
#[derive(Deserialize)]
pub struct CallbackQuery {
    state: String,
    code: Option<String>,
    error: Option<String>,
    #[serde(rename = "session_state")]
    _session_state: Option<String>,
    #[serde(rename = "iss")]
    iss: Option<String>,
}
async fn callback(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let Query(query) = query.map_err(|_| AdminError::Invalid)?;
    if query.error.is_some() {
        return Err(AdminError::Unauthenticated);
    }
    if query
        .iss
        .as_ref()
        .is_some_and(|issuer| issuer != &identity.config.issuer)
    {
        return Err(AdminError::Unauthenticated);
    }
    let browser = cookie(&headers, FLOW_COOKIE)?;
    let token = identity
        .callback(
            &query.state,
            query.code.as_deref().ok_or(AdminError::Invalid)?,
            &browser,
        )
        .await?;
    let mut response = Redirect::to("/overview").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(SESSION_COOKIE, &token, 8 * 3600)?,
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(FLOW_COOKIE, "", 0)?);
    Ok(response)
}
async fn step_up(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let current = identity.current(&headers).await?;
    identity.check_write(&headers, &current)?;
    let login = identity.start(Some(&current)).await?;
    let mut response = Json(json!({"authorization_url":login.authorization_url})).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(FLOW_COOKIE, &login.browser_secret, 300)?,
    );
    Ok(response)
}
async fn session(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, AdminError> {
    Ok(Json(identity.current(&headers).await?.view))
}
async fn logout(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    identity.revoke_self(&headers).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0)?);
    Ok(response)
}
async fn revocation_context(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
) -> Result<Json<orange_protocol::user::SessionRevocationContext>, AdminError> {
    Ok(Json(identity.revocation_context(&headers).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
    limit: Option<u16>,
    cursor: Option<String>,
}
impl ScopedQuery {
    pub(super) fn scope(&self) -> Result<Scope, AdminError> {
        if !matches!(self.environment.as_str(), "dev" | "staging" | "production")
            || !matches!(
                self.scope_kind.as_str(),
                "platform" | "tenant" | "region" | "resource"
            )
            || !orange_protocol::valid_id(&self.scope_id)
        {
            return Err(AdminError::Invalid);
        }
        if self.scope_kind == "platform" && self.scope_id != "platform" {
            return Err(AdminError::Invalid);
        }
        Ok(Scope {
            environment: self.environment.clone(),
            kind: self.scope_kind.clone(),
            scope_id: self.scope_id.clone(),
        })
    }
}
fn readable_actions(permissions: &BTreeSet<String>) -> Vec<String> {
    [
        ("run.cancel", "runs.read"),
        ("case.update", "cases.read"),
        ("node.drain", "nodes.read"),
        ("node.quarantine", "nodes.read"),
        ("node.resume", "nodes.read"),
        ("model.activate", "models.read"),
        ("model.disable", "models.read"),
        ("account.restrict", "risk.read"),
        ("credential.revoke", "risk.read"),
        ("payment.refund", "payments.read"),
        ("ledger.reverse", "ledger.read"),
        ("pricing.publish", "pricing.read"),
        ("release.rollout", "releases.read"),
        ("release.revoke", "releases.read"),
        ("feature.disable", "features.read"),
        ("feature.enable", "features.read"),
        ("admin.role_grant", "audit.read"),
        ("support_access.grant", "support.read"),
        ("support_access.revoke", "support.read"),
    ]
    .into_iter()
    .filter(|(_, permission)| permissions.contains(*permission))
    .map(|(kind, _)| kind.to_owned())
    .collect()
}
async fn read_audit(
    identity: &AdminIdentity,
    session: &CurrentSession,
    scope: &Scope,
    kind: &str,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,occurred_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&session.view.principal_id).bind(kind).bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(session.now).execute(&identity.pool).await?;
    Ok(())
}
async fn overview(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = query.scope()?;
    if query.cursor.is_some()
        || query.limit.is_some()
        || !matches!(scope.kind.as_str(), "platform" | "tenant")
    {
        return Err(AdminError::Invalid);
    }
    let current = identity.current(&headers).await?;
    current.authorize("overview.read", &scope)?;
    let owner = if scope.kind == "tenant" {
        Some(scope.scope_id.as_str())
    } else {
        None
    };
    let permissions = current.grants.get(&scope).ok_or(AdminError::Forbidden)?;
    let kinds = readable_actions(permissions);
    // Existing jobs belong to this single-environment deployment; scope is still owner-filtered.
    let jobs=sqlx::query("SELECT count(*) FILTER(WHERE execution_state='queued')::bigint AS queued,count(*) FILTER(WHERE execution_state='running')::bigint AS running FROM jobs WHERE ($1::text IS NULL OR owner_id=$1)")
        .bind(owner).fetch_one(&identity.pool).await?;
    let unknown:i64=sqlx::query_scalar("SELECT count(*) FROM admin_actions WHERE environment=$1 AND ($2::text IS NULL OR (scope_kind='tenant' AND scope_id=$2)) AND action_type=ANY($3) AND document->>'state'='unknown'")
        .bind(&scope.environment).bind(owner).bind(kinds).fetch_one(&identity.pool).await?;
    let cases:i64=sqlx::query_scalar("SELECT count(*) FROM reconciliation_cases WHERE environment=$1 AND ($2::text IS NULL OR owner_id=$2) AND state!='resolved'")
        .bind(&scope.environment).bind(owner).fetch_one(&identity.pool).await?;
    let nodes:i64=sqlx::query_scalar("SELECT count(*) FROM nodes WHERE environment=$1 AND ($2::text IS NULL OR owner_id=$2) AND state='quarantined'")
        .bind(&scope.environment).bind(owner).fetch_one(&identity.pool).await?;
    // Revocation after the initial read cannot authorize the returned content.
    let refreshed = identity.current(&headers).await?;
    refreshed.authorize("overview.read", &scope)?;
    if refreshed.grants.get(&scope) != Some(permissions) {
        return Err(AdminError::Forbidden);
    }
    read_audit(&identity, &current, &scope, "overview.read").await?;
    Ok(Json(
        json!({"as_of":timestamp(current.now)?,"scope":scope,"queued_jobs":jobs.try_get::<i64,_>("queued")?,"running_jobs":jobs.try_get::<i64,_>("running")?,"unknown_actions":unknown,"open_reconciliation_cases":cases,"quarantined_nodes":nodes}),
    ))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    authority: String,
    last_id: String,
}
async fn actions(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = query.scope()?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let current = identity.current(&headers).await?;
    current.authorize("actions.read", &scope)?;
    let permissions = current.grants.get(&scope).ok_or(AdminError::Forbidden)?;
    let authority = hash_secret(
        &serde_json::to_string(&(
            &current.view.principal_id,
            &current.view.policy_version,
            &scope,
            permissions,
        ))
        .map_err(|_| AdminError::Invalid)?,
    );
    let after = if let Some(cursor) = query.cursor {
        if cursor.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let value: Cursor = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| AdminError::Invalid)?,
        )
        .map_err(|_| AdminError::Invalid)?;
        if value.authority != authority || !orange_protocol::valid_id(&value.last_id) {
            return Err(AdminError::Invalid);
        }
        value.last_id
    } else {
        String::new()
    };
    let rows=sqlx::query("SELECT action_id,document::text AS document FROM admin_actions WHERE environment=$1 AND ($6='platform' OR (scope_kind=$6 AND scope_id=$2)) AND action_type=ANY($3) AND action_id>$4 ORDER BY action_id LIMIT $5")
        .bind(&scope.environment).bind(&scope.scope_id).bind(readable_actions(permissions)).bind(after).bind(i64::from(limit)+1).bind(&scope.kind).fetch_all(&identity.pool).await?;
    let more = rows.len() > usize::from(limit);
    let mut data = Vec::new();
    let mut last = String::new();
    for row in rows.into_iter().take(usize::from(limit)) {
        last = row.try_get("action_id")?;
        let raw: String = row.try_get("document")?;
        data.push(serde_json::from_str::<Value>(&raw).map_err(|_| AdminError::Unavailable)?);
    }
    let refreshed = identity.current(&headers).await?;
    refreshed.authorize("actions.read", &scope)?;
    if refreshed.grants.get(&scope) != Some(permissions) {
        return Err(AdminError::Forbidden);
    }
    read_audit(&identity, &current, &scope, "actions.read").await?;
    let mut value = json!({"data":data});
    if more {
        value["next_cursor"] = Value::String(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Cursor {
                    authority,
                    last_id: last,
                })
                .map_err(|_| AdminError::Invalid)?,
            ),
        );
    }
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn financial_actions_are_not_exposed_by_generic_action_permission() {
        let permissions = BTreeSet::from(["actions.read".into(), "nodes.read".into()]);
        let actions = readable_actions(&permissions);
        assert!(actions.contains(&"node.drain".into()));
        assert!(!actions.contains(&"payment.refund".into()));
    }
    #[test]
    fn duplicate_cookies_and_malformed_scope_are_rejected() {
        let secret = random_secret();
        let mut valid = HeaderMap::new();
        valid.insert(
            "cookie",
            format!("{SESSION_COOKIE}={secret}")
                .parse()
                .expect("header"),
        );
        assert_eq!(
            cookie(&valid, SESSION_COOKIE).expect("generated cookie"),
            secret
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!(
                "{SESSION_COOKIE}={}; {SESSION_COOKIE}={}",
                "a".repeat(40),
                "b".repeat(40)
            )
            .parse()
            .expect("header"),
        );
        assert!(cookie(&headers, SESSION_COOKIE).is_err());
        let bad = ScopedQuery {
            environment: "production".into(),
            scope_kind: "platform".into(),
            scope_id: "arbitrary".into(),
            limit: None,
            cursor: None,
        };
        assert!(bad.scope().is_err());
    }
}
