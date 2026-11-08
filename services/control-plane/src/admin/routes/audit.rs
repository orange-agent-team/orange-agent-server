//! Read-only scoped event metadata. A recorded event never asserts completion
//! of the underlying Job, cleanup, payment or reconciliation.
use super::*;
use axum::extract::Path;
fn request_id(headers: &HeaderMap) -> Result<String, AdminError> {
    if headers.get_all("x-request-id").iter().count() > 1 {
        return Err(AdminError::Invalid);
    }
    match headers.get("x-request-id") {
        Some(value) => value
            .to_str()
            .ok()
            .filter(|s| orange_protocol::valid_id(s))
            .map(str::to_owned)
            .ok_or(AdminError::Invalid),
        None => Ok(uuid::Uuid::new_v4().to_string()),
    }
}
fn response(value: Value, request: &str) -> Result<Response, AdminError> {
    let mut result = Json(value).into_response();
    result.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(request).map_err(|_| AdminError::Unavailable)?,
    );
    Ok(result)
}

fn authority(current: &CurrentSession, scope: &Scope) -> Result<String, AdminError> {
    current.authorize("audit.read", scope)?;
    Ok(hash_secret(
        &serde_json::to_string(&(
            "admin-audit-cursor-v1",
            &current.view.principal_id,
            &current.view.policy_version,
            scope,
            current.grants.get(scope).ok_or(AdminError::Forbidden)?,
        ))
        .map_err(|_| AdminError::Unavailable)?,
    ))
}
fn project(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    let scope = ScopedQuery {
        environment: row.try_get("scope_environment")?,
        scope_kind: row.try_get("scope_kind")?,
        scope_id: row.try_get("scope_id")?,
        limit: None,
        cursor: None,
    }
    .scope()
    .map_err(|_| AdminError::Unavailable)?;
    let id: String = row.try_get("event_id")?;
    let actor: String = row.try_get("principal_id")?;
    let kind: String = row.try_get("event_type")?;
    if !orange_protocol::valid_id(&id)
        || !orange_protocol::valid_id(&actor)
        || kind.trim().is_empty()
        || kind.len() > 100
    {
        return Err(AdminError::Unavailable);
    }
    let mut value = json!({"event_id":id,"actor_id":actor,"scope":scope,
        "event_type":kind,"result":"recorded","occurred_at":timestamp(row.try_get("occurred_at")?)?,
        "request_id":row.try_get::<Option<String>,_>("request_id")?,"version":"record-v1"});
    // These existing writers identify their action explicitly. Node reads and
    // session events are not relabelled as action records.
    if kind.starts_with("action.") {
        let id: String = row
            .try_get::<Option<String>, _>("entity_id")?
            .ok_or(AdminError::Unavailable)?;
        if !orange_protocol::valid_id(&id) {
            return Err(AdminError::Unavailable);
        }
        value["action_id"] = json!(id);
    }
    Ok(value)
}
async fn record_read(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: &CurrentSession,
    scope: &Scope,
    kind: &str,
    entity: Option<&str>,
    request: &str,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,request_id,occurred_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&current.view.principal_id).bind(kind)
        .bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(entity).bind(request)
        .execute(&mut **tx).await?;
    Ok(())
}
async fn next_cursor(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_hash: &str,
    current: &CurrentSession,
    fingerprint: &str,
    snapshot: i64,
    before: i64,
) -> Result<String, AdminError> {
    // Serialize quota decisions for this operator without retaining a model or
    // user-controlled process handle. Expired metadata is pruned in a bounded batch.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('admin-audit-cursors:' || $1,11))")
        .bind(&current.view.principal_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM admin_audit_cursors WHERE cursor_hash IN (SELECT cursor_hash FROM admin_audit_cursors WHERE expires_at<=FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint ORDER BY expires_at LIMIT 100 FOR UPDATE SKIP LOCKED)")
        .execute(&mut **tx).await?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM admin_audit_cursors WHERE principal_id=$1 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
        .bind(&current.view.principal_id).fetch_one(&mut **tx).await?;
    if count >= 256 {
        return Err(AdminError::Unavailable);
    }
    let token = random_secret();
    sqlx::query("INSERT INTO admin_audit_cursors(cursor_hash,session_hash,principal_id,authority,snapshot_sequence,before_sequence,expires_at) VALUES($1,$2,$3,$4,$5,$6,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+300)")
        .bind(hash_secret(&token)).bind(session_hash).bind(&current.view.principal_id)
        .bind(fingerprint).bind(snapshot).bind(before).execute(&mut **tx).await?;
    Ok(token)
}
pub(super) async fn list(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = request_id(&headers)?;
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = query.scope()?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let current = identity.current(&headers).await?;
    let fingerprint = authority(&current, &scope)?;
    let session_hash = hash_secret(&cookie(&headers, SESSION_COOKIE)?);
    let mut tx = identity.pool.begin().await?;
    let (snapshot, before) = if let Some(token) = query.cursor {
        if token.is_empty() || token.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let row=sqlx::query("SELECT snapshot_sequence,before_sequence FROM admin_audit_cursors WHERE cursor_hash=$1 AND session_hash=$2 AND principal_id=$3 AND authority=$4 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .bind(hash_secret(&token)).bind(&session_hash).bind(&current.view.principal_id).bind(&fingerprint)
            .fetch_optional(&mut *tx).await?.ok_or(AdminError::Invalid)?;
        (
            row.try_get::<i64, _>("snapshot_sequence")?,
            Some(row.try_get::<i64, _>("before_sequence")?),
        )
    } else {
        let upper:i64=sqlx::query_scalar("SELECT COALESCE(max(audit_sequence),0) FROM admin_audit WHERE scope_environment=$1 AND scope_kind IS NOT NULL AND scope_id IS NOT NULL AND ($2='platform' OR (scope_kind=$2 AND scope_id=$3))")
            .bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).fetch_one(&mut *tx).await?;
        (upper, None)
    };
    let rows=sqlx::query("SELECT event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,request_id,occurred_at,audit_sequence FROM admin_audit WHERE scope_environment=$1 AND scope_kind IS NOT NULL AND scope_id IS NOT NULL AND ($2='platform' OR (scope_kind=$2 AND scope_id=$3)) AND audit_sequence<=$4 AND ($5::bigint IS NULL OR audit_sequence<$5) ORDER BY audit_sequence DESC LIMIT $6")
        .bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(snapshot).bind(before)
        .bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let more = rows.len() > usize::from(limit);
    let mut data = Vec::new();
    let mut last = None;
    for row in rows.into_iter().take(usize::from(limit)) {
        last = Some(row.try_get::<i64, _>("audit_sequence")?);
        data.push(project(&row)?);
    }
    let mut result = json!({"data":data});
    if more {
        result["next_cursor"] = json!(
            next_cursor(
                &mut tx,
                &session_hash,
                &current,
                &fingerprint,
                snapshot,
                last.ok_or(AdminError::Unavailable)?
            )
            .await?
        );
    }
    let refreshed = identity.current(&headers).await?;
    if authority(&refreshed, &scope)? != fingerprint {
        return Err(AdminError::Forbidden);
    }
    record_read(&mut tx, &refreshed, &scope, "audit.list", None, &request).await?;
    tx.commit().await?;
    response(result, &request)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetailQuery {
    environment: String,
}
pub(super) async fn get_one(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<DetailQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = request_id(&headers)?;
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    if !orange_protocol::valid_id(&id)
        || !matches!(query.environment.as_str(), "dev" | "staging" | "production")
    {
        return Err(AdminError::Invalid);
    }
    let current = identity.current(&headers).await?;
    if !current
        .grants
        .iter()
        .any(|(s, p)| s.environment == query.environment && p.contains("audit.read"))
    {
        return Err(AdminError::Forbidden);
    }
    let mut tx = identity.pool.begin().await?;
    let row=sqlx::query("SELECT event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,request_id,occurred_at FROM admin_audit WHERE event_id=$1 AND scope_environment=$2 AND scope_kind IS NOT NULL AND scope_id IS NOT NULL")
        .bind(&id).bind(&query.environment).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?;
    // Decide scope before projecting any other row metadata or reporting a
    // malformed foreign row. Unauthorized identifiers remain undisclosed.
    let row_scope = ScopedQuery {
        environment: row.try_get("scope_environment")?,
        scope_kind: row.try_get("scope_kind")?,
        scope_id: row.try_get("scope_id")?,
        limit: None,
        cursor: None,
    }
    .scope()
    .map_err(|_| AdminError::NotFound)?;
    let platform = Scope {
        environment: query.environment,
        kind: "platform".into(),
        scope_id: "platform".into(),
    };
    let authorized_scope = if current.authorize("audit.read", &row_scope).is_ok() {
        row_scope
    } else if current.authorize("audit.read", &platform).is_ok() {
        platform
    } else {
        return Err(AdminError::NotFound);
    };
    let value = project(&row)?;
    let fingerprint = authority(&current, &authorized_scope)?;
    let refreshed = identity.current(&headers).await?;
    if authority(&refreshed, &authorized_scope)? != fingerprint {
        return Err(AdminError::Forbidden);
    }
    record_read(
        &mut tx,
        &refreshed,
        &authorized_scope,
        "audit.read",
        Some(&id),
        &request,
    )
    .await?;
    tx.commit().await?;
    response(value, &request)
}

/// The legacy session permission intersection omits scope-specific rights. This additive
/// endpoint lets the UI render conservative controls for the selected scope.
pub(super) async fn scope_permissions(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = request_id(&headers)?;
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    if query.limit.is_some() || query.cursor.is_some() {
        return Err(AdminError::Invalid);
    }
    let scope = query.scope()?;
    let current = identity.current(&headers).await?;
    let permissions = current.grants.get(&scope).ok_or(AdminError::Forbidden)?;
    let refreshed = identity.current(&headers).await?;
    if refreshed.view.principal_id != current.view.principal_id
        || refreshed.view.policy_version != current.view.policy_version
        || refreshed.grants.get(&scope) != Some(permissions)
    {
        return Err(AdminError::Forbidden);
    }
    let value = json!({"principal_id":current.view.principal_id,"scope":scope,"permissions":permissions,"policy_version":current.view.policy_version});
    let mut tx = identity.pool.begin().await?;
    record_read(
        &mut tx,
        &refreshed,
        &scope,
        "session.scope_read",
        None,
        &request,
    )
    .await?;
    tx.commit().await?;
    response(value, &request)
}
