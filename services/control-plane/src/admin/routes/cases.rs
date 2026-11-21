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

pub(super) async fn activity(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let query = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = query.scope()?;
    let limit = query.limit.unwrap_or(50);
    if !id_valid(&id)
        || !(1..=200).contains(&limit)
        || !matches!(scope.kind.as_str(), "tenant" | "platform")
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("cases.read", &scope)?;
    let owner = (scope.kind == "tenant").then_some(&scope.scope_id);
    let row=sqlx::query("SELECT owner_id,version,assigned_to,resolution_ref FROM reconciliation_cases WHERE environment=$1 AND case_id=$2 AND ($3::text IS NULL OR owner_id=$3) FOR SHARE")
        .bind(&scope.environment).bind(&id).bind(owner).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?;
    let session_hash = hash_secret(&cookie(&headers, SESSION_COOKIE)?);
    let fingerprint = hash_secret(
        &serde_json::to_string(&(
            "case-activity.v1",
            &scope,
            &id,
            &session.view.principal_id,
            &session.view.policy_version,
            session.grants.get(&scope),
        ))
        .map_err(|_| AdminError::Unavailable)?,
    );
    let version: i64 = row.try_get("version")?;
    let (upper, before) = if let Some(token) = query.cursor {
        if token.is_empty() || token.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let (upper,last):(i64,i64)=sqlx::query_as("SELECT snapshot_sequence,before_sequence FROM admin_audit_cursors WHERE cursor_hash=$1 AND session_hash=$2 AND authority=$3 AND principal_id=$4 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint").bind(hash_secret(&token)).bind(&session_hash).bind(&fingerprint).bind(&session.view.principal_id).fetch_optional(&mut *tx).await?.ok_or(AdminError::Invalid)?;
        (upper, Some(last))
    } else {
        (version, None)
    };
    let rows=sqlx::query("SELECT e.action_id,e.actor_id,e.occurred_at,e.case_version,a.document->'request'->'payload' AS payload FROM reconciliation_case_edits e JOIN admin_action_events a ON a.action_id=e.action_id AND a.kind='action.execute' AND a.principal_id=e.actor_id WHERE case_id=$1 AND case_version<=$2 AND ($3::bigint IS NULL OR case_version<$3) ORDER BY case_version DESC LIMIT $4")
        .bind(&id).bind(upper).bind(before).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let mut data = Vec::new();
    let mut last = None;
    for event in rows.iter().take(usize::from(limit)) {
        let rev = event.try_get::<i64, _>("case_version")?;
        let payload = event.try_get::<Value, _>("payload")?;
        if payload["case_id"] != id {
            return Err(AdminError::Unavailable);
        }
        data.push(json!({"action_id":identifier(event.try_get("action_id")?)?,"actor_id":identifier(event.try_get("actor_id")?)?,"occurred_at":timestamp(event.try_get("occurred_at")?)?,"version":rev.to_string(),"payload":payload}));
        last = Some(rev);
    }
    let mut result = json!({"case_id":id,"owner_id":identifier(row.try_get("owner_id")?)?,"version":version.to_string(),"data":data});
    if scope.environment == "dev" {
        let receipt:Option<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('event_id',r.event_id,'state',r.state,
            'generation',r.generation::text,'attempts',r.attempts::text,'policy_id',r.policy_id,
            'retry_deadline_unix',floor(extract(epoch FROM r.deadline))::bigint::text,
            'last_failure',(SELECT h.reason FROM metering_receipt_history h WHERE h.event_id=r.event_id AND h.generation=r.generation
               AND h.stage IN('retry','dead') ORDER BY h.sequence_id DESC LIMIT 1))
            FROM metering_receipts r WHERE r.case_id=$1 AND r.owner_id=$2")
            .bind(&id).bind(row.try_get::<&str,_>("owner_id")?).fetch_optional(&mut *tx).await?;
        if let Some(receipt) = receipt {
            result["metering"] = receipt;
        }
    }
    for field in ["assigned_to", "resolution_ref"] {
        if let Some(value) = row.try_get::<Option<String>, _>(field)? {
            result[field] = json!(identifier(value)?);
        }
    }
    if rows.len() > usize::from(limit) {
        result["next_cursor"] = json!(
            audit::next_cursor(
                &mut tx,
                &session_hash,
                &session,
                &fingerprint,
                upper,
                last.ok_or(AdminError::Unavailable)?
            )
            .await?
        );
    }
    let live = identity.current_in(&headers, &mut tx).await?;
    live.authorize("cases.read", &scope)?;
    audit::record_read(
        &mut tx,
        &live,
        &scope,
        "reconciliation_case.activity",
        Some(&id),
        &request,
    )
    .await?;
    tx.commit().await?;
    audit::response(result, &request)
}
fn id_valid(value: &str) -> bool {
    orange_protocol::valid_id(value) && value.as_bytes()[0].is_ascii_alphanumeric()
}
