//! Whole-merchant reports require finance permissions at platform scope.
//! Tenant operators can inspect a single finding only through its owned reconciliation case.
use super::*;
use axum::extract::Path;

fn platform(q: &ScopedQuery) -> Result<Scope, AdminError> {
    let scope = q.scope()?;
    if scope.environment != "dev" || scope.kind != "platform" {
        return Err(AdminError::Forbidden);
    }
    Ok(scope)
}
fn finance(session: &crate::admin::CurrentSession, scope: &Scope) -> Result<(), AdminError> {
    for p in ["payments.read", "ledger.read", "cases.read"] {
        session.authorize(p, scope)?;
    }
    Ok(())
}
pub(super) async fn list(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    q: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    let scope = platform(&q)?;
    let n = q.limit.unwrap_or(20);
    if !(1..=100).contains(&n) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    finance(&session, &scope)?;
    let before: Option<i64> = match q.cursor {
        Some(id) => Some(
            sqlx::query_scalar("SELECT sequence_id FROM test_statement_runs WHERE run_id=$1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AdminError::Invalid)?,
        ),
        None => None,
    };
    let rows=sqlx::query("SELECT r.run_id,r.merchant_id,r.from_unix,r.until_unix,r.completed_at IS NOT NULL AS sealed,s.last_error FROM test_statement_runs r LEFT JOIN test_statement_sync s ON r.run_id=s.run_id WHERE ($1::bigint IS NULL OR r.sequence_id<$1) ORDER BY sequence_id DESC LIMIT $2")
        .bind(before).bind(i64::from(n)+1).fetch_all(&mut *tx).await?;
    let data=rows.iter().take(n as usize).map(|r|Ok(json!({"run_id":r.try_get::<&str,_>("run_id")?,"merchant_id":r.try_get::<&str,_>("merchant_id")?,
        "scan_from_unix":r.try_get::<i64,_>("from_unix")?.to_string(),"until_unix":r.try_get::<i64,_>("until_unix")?.to_string(),"sealed":r.try_get::<bool,_>("sealed")?,"last_error":r.try_get::<Option<&str>,_>("last_error")?}))).collect::<Result<Vec<Value>,AdminError>>()?;
    let cursor = (rows.len() > n as usize)
        .then(|| data.last().and_then(|v| v["run_id"].as_str()))
        .flatten();
    finance(&identity.current_in(&headers, &mut tx).await?, &scope)?;
    audit::record_read(&mut tx, &session, &scope, "statements.list", None, &request).await?;
    let result = json!({"data":data,"next_cursor":cursor});
    tx.commit().await?;
    audit::response(result, &request)
}
pub(super) async fn detail(
    State(identity): State<Arc<AdminIdentity>>,
    Path(run): Path<String>,
    headers: HeaderMap,
    q: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    let scope = platform(&q)?;
    let n = q.limit.unwrap_or(50);
    if !(1..=100).contains(&n)
        || !orange_protocol::valid_id(&run)
        || q.cursor
            .as_deref()
            .is_some_and(|id| !orange_protocol::valid_digest(id))
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let s = identity.current_in(&headers, &mut tx).await?;
    finance(&s, &scope)?;
    let row=sqlx::query("SELECT merchant_id,report FROM test_statement_runs WHERE run_id=$1 AND completed_at IS NOT NULL").bind(&run).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?;
    let issues=sqlx::query("SELECT i.issue_key,i.kind,i.business_ref,i.case_id,i.owner_id FROM test_statement_issues i WHERE i.merchant_id=$1 AND i.issue_key>$3 AND EXISTS
        (SELECT 1 FROM test_statement_issue_evidence e WHERE e.run_id=$2 AND e.issue_key=i.issue_key AND e.merchant_id=i.merchant_id) ORDER BY i.issue_key LIMIT $4")
        .bind(row.try_get::<&str,_>("merchant_id")?).bind(&run).bind(q.cursor.unwrap_or_default()).bind(i64::from(n)+1).fetch_all(&mut *tx).await?;
    let data=issues.iter().take(n as usize).map(|r|Ok(json!({"issue_key":r.try_get::<&str,_>("issue_key")?,"kind":r.try_get::<&str,_>("kind")?,"business_ref":r.try_get::<&str,_>("business_ref")?,
        "case_id":r.try_get::<Option<&str>,_>("case_id")?,"owner_id":r.try_get::<Option<&str>,_>("owner_id")?}))).collect::<Result<Vec<Value>,AdminError>>()?;
    let cursor = (issues.len() > n as usize)
        .then(|| data.last().and_then(|v| v["issue_key"].as_str()))
        .flatten();
    let result = json!({"run_id":run,"merchant_id":row.try_get::<&str,_>("merchant_id")?,"report":row.try_get::<Value,_>("report")?,"issues":data,"next_cursor":cursor});
    finance(&identity.current_in(&headers, &mut tx).await?, &scope)?;
    audit::record_read(&mut tx, &s, &scope, "statements.read", Some(&run), &request).await?;
    tx.commit().await?;
    audit::response(result, &request)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidenceQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
    run_id: Option<String>,
    cursor: Option<String>,
    limit: Option<u16>,
}
pub(super) async fn evidence(
    State(identity): State<Arc<AdminIdentity>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    q: Result<Query<EvidenceQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    let scope = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    let n = q.limit.unwrap_or(20);
    if scope.environment != "dev"
        || !(scope.kind == "tenant" || scope.kind == "platform" && scope.scope_id == "platform")
        || !orange_protocol::valid_id(&scope.scope_id)
        || !orange_protocol::valid_digest(&key)
        || q.run_id
            .as_deref()
            .is_some_and(|id| !orange_protocol::valid_id(id))
        || !(1..=100).contains(&n)
        || q.cursor
            .as_deref()
            .is_some_and(|id| !orange_protocol::valid_digest(id))
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    finance(&session, &scope)?;
    let owner = (scope.kind == "tenant").then_some(scope.scope_id.as_str());
    let run = if let Some(run) = q.run_id {
        run
    } else {
        sqlx::query_scalar("SELECT r.run_id FROM test_statement_runs r JOIN test_statement_issue_evidence e USING(run_id,merchant_id) JOIN test_statement_issues i USING(issue_key,merchant_id)
            WHERE i.issue_key=$1 AND ($2::text IS NULL OR i.owner_id=$2) AND r.completed_at IS NOT NULL ORDER BY r.sequence_id DESC LIMIT 1")
            .bind(&key).bind(owner).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?
    };
    let issue=sqlx::query("SELECT i.* FROM test_statement_issues i JOIN test_statement_runs r USING(merchant_id)
       WHERE r.run_id=$1 AND r.completed_at IS NOT NULL AND i.issue_key=$2 AND ($3::text IS NULL OR i.owner_id=$3)
       AND EXISTS(SELECT 1 FROM test_statement_issue_evidence e WHERE e.run_id=$1 AND e.issue_key=i.issue_key AND e.merchant_id=i.merchant_id)")
        .bind(&run).bind(&key).bind(owner).fetch_optional(&mut *tx).await?.ok_or(AdminError::NotFound)?;
    let rows=sqlx::query("SELECT evidence_digest,detail FROM test_statement_issue_evidence WHERE run_id=$1 AND issue_key=$2 AND evidence_digest>$3 ORDER BY evidence_digest LIMIT $4")
        .bind(&run).bind(&key).bind(q.cursor.unwrap_or_default()).bind(i64::from(n)+1).fetch_all(&mut *tx).await?;
    let data=rows.iter().take(n as usize).map(|r|Ok(json!({"digest":r.try_get::<&str,_>("evidence_digest")?,"detail":r.try_get::<Value,_>("detail")?}))).collect::<Result<Vec<Value>,AdminError>>()?;
    let next = (rows.len() > n as usize)
        .then(|| data.last().and_then(|v| v["digest"].as_str()))
        .flatten();
    let result = json!({"run_id":run,"issue_key":key,"kind":issue.try_get::<&str,_>("kind")?,"owner_id":issue.try_get::<Option<&str>,_>("owner_id")?,
        "case_id":issue.try_get::<Option<&str>,_>("case_id")?,"business_ref":issue.try_get::<&str,_>("business_ref")?,"data":data,"next_cursor":next});
    finance(&identity.current_in(&headers, &mut tx).await?, &scope)?;
    audit::record_read(
        &mut tx,
        &session,
        &scope,
        "statement_evidence.read",
        Some(&key),
        &request,
    )
    .await?;
    tx.commit().await?;
    audit::response(result, &request)
}
