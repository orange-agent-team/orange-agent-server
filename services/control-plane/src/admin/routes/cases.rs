//! Reconciliation case metadata visible to the selected finance or audit scope.
use super::*;
use axum::extract::Path;

fn identifier(value: String) -> Result<String, AdminError> {
    if !orange_protocol::valid_id(&value) || !value.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(AdminError::Unavailable);
    }
    Ok(value)
}
fn project(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    let state: String = row.try_get("state")?;
    let version: i64 = row.try_get("version")?;
    if version < 1
        || !matches!(
            state.as_str(),
            "open" | "investigating" | "waiting_external" | "resolved"
        )
    {
        return Err(AdminError::Unavailable);
    }
    Ok(
        json!({"case_id":identifier(row.try_get("case_id")?)?,"owner_id":identifier(row.try_get("owner_id")?)?,
        "business_ref":identifier(row.try_get("business_ref")?)?,"state":state,
        "next_check_at":timestamp(row.try_get("next_check_at")?)?,"version":version.to_string()}),
    )
}
pub(super) async fn list(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = query.scope()?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) || !matches!(scope.kind.as_str(), "platform" | "tenant") {
        return Err(AdminError::Invalid);
    }
    let owner = (scope.kind == "tenant").then_some(scope.scope_id.as_str());
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("cases.read", &scope)?;
    let session_hash = hash_secret(&cookie(&headers, SESSION_COOKIE)?);
    let grants = session.grants.get(&scope).ok_or(AdminError::Forbidden)?;
    let fingerprint = hash_secret(
        &serde_json::to_string(&(
            "reconciliation-cases.v1",
            &session.view.principal_id,
            &session.view.policy_version,
            &scope,
            grants,
        ))
        .map_err(|_| AdminError::Unavailable)?,
    );
    let (upper, before) = if let Some(token) = query.cursor {
        if token.is_empty() || token.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let bounds:(i64,i64)=sqlx::query_as("SELECT snapshot_sequence,before_sequence FROM admin_audit_cursors WHERE cursor_hash=$1 AND session_hash=$2 AND principal_id=$3 AND authority=$4 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .bind(hash_secret(&token)).bind(&session_hash).bind(&session.view.principal_id).bind(&fingerprint).fetch_optional(&mut *tx).await?.ok_or(AdminError::Invalid)?;
        (bounds.0, Some(bounds.1))
    } else {
        let upper:i64=sqlx::query_scalar("SELECT coalesce(max(case_sequence),0) FROM reconciliation_cases WHERE environment=$1 AND ($2::text IS NULL OR owner_id=$2)").bind(&scope.environment).bind(owner).fetch_one(&mut *tx).await?;
        (upper, None)
    };
    let rows=sqlx::query("SELECT case_id,owner_id,business_ref,state,next_check_at,version,case_sequence FROM reconciliation_cases WHERE environment=$1 AND ($2::text IS NULL OR owner_id=$2) AND case_sequence<=$3 AND ($4::bigint IS NULL OR case_sequence<$4) ORDER BY case_sequence DESC LIMIT $5")
        .bind(&scope.environment).bind(owner).bind(upper).bind(before).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let visible = &rows[..rows.len().min(usize::from(limit))];
    let data = visible.iter().map(project).collect::<Result<Vec<_>, _>>()?;
    let mut result = json!({"data":data});
    if visible.len() < rows.len() {
        let last = visible
            .last()
            .ok_or(AdminError::Unavailable)?
            .try_get("case_sequence")?;
        result["next_cursor"] = json!(
            audit::next_cursor(&mut tx, &session_hash, &session, &fingerprint, upper, last).await?
        );
    }
    let live = identity.current_in(&headers, &mut tx).await?;
    live.authorize("cases.read", &scope)?;
    audit::record_read(
        &mut tx,
        &session,
        &scope,
        "reconciliation_cases.list",
        None,
        &request,
    )
    .await?;
    tx.commit().await?;
    audit::response(result, &request)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CaseDetailQuery {
    environment: String,
}
pub(super) async fn detail(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<CaseDetailQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let env = query.map_err(|_| AdminError::Invalid)?.environment.clone();
    if !matches!(env.as_str(), "dev" | "staging" | "production")
        || !orange_protocol::valid_id(&id)
        || !id.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    let row=sqlx::query("SELECT case_id,owner_id,business_ref,state,next_check_at,version FROM reconciliation_cases WHERE environment=$1 AND case_id=$2").bind(&env).bind(&id).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?;
    let tenant = Scope {
        environment: env.clone(),
        kind: "tenant".into(),
        scope_id: row.try_get("owner_id")?,
    };
    let platform = Scope {
        environment: env,
        kind: "platform".into(),
        scope_id: "platform".into(),
    };
    let scope = if session.authorize("cases.read", &tenant).is_ok() {
        tenant
    } else if session.authorize("cases.read", &platform).is_ok() {
        platform
    } else {
        return Err(AdminError::NotFound);
    };
    let result = project(&row)?;
    let live = identity.current_in(&headers, &mut tx).await?;
    live.authorize("cases.read", &scope)?;
    audit::record_read(
        &mut tx,
        &session,
        &scope,
        "reconciliation_case.read",
        Some(&id),
        &request,
    )
    .await?;
    tx.commit().await?;
    audit::response(result, &request)
}
