//! Live scoped metadata; client-certificate validity is not node readiness or execution capacity.
use super::*;

const PERMISSION: &str = "device_issuers.read";

fn position(current: &CurrentSession, scope: &Scope, after: &str) -> Result<String, AdminError> {
    let permissions = current.grants.get(scope).ok_or(AdminError::Forbidden)?;
    let binding = orange_protocol::request_digest(
        "admin.device-issuer-position.v1",
        &(
            &current.view.principal_id,
            &current.view.policy_version,
            scope,
            permissions,
            after,
        ),
    )
    .map_err(|_| AdminError::Unavailable)?;
    Ok(format!("{after}.{binding}"))
}

pub(super) async fn list(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    q: Result<Query<ScopedQuery>, QueryRejection>,
) -> Result<Response, AdminError> {
    let request = audit::request_id(&headers)?;
    let q = q.map_err(|_| AdminError::Invalid)?.0;
    let scope = q.scope()?;
    let limit = q.limit.unwrap_or(20);
    if scope.environment != "dev"
        || !matches!(scope.kind.as_str(), "platform" | "region")
        || !(1..=100).contains(&limit)
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize(PERMISSION, &scope)?;
    let after = if let Some(cursor) = &q.cursor {
        let (last, tag) = cursor.split_once('.').ok_or(AdminError::Invalid)?;
        if !orange_protocol::valid_digest(last)
            || !orange_protocol::valid_digest(tag)
            || position(&session, &scope, last)? != *cursor
        {
            return Err(AdminError::Invalid);
        }
        Some(last)
    } else {
        None
    };
    let region = (scope.kind == "region").then_some(scope.scope_id.as_str());
    let at: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(&mut *tx)
        .await?;
    let rows=sqlx::query(r"
        WITH page AS MATERIALIZED (
            SELECT e.issuer_sha256 FROM worker_enrollment_certificates e
            WHERE ($1::text IS NULL OR e.region_id=$1) AND ($2::text IS NULL OR e.issuer_sha256>$2)
            GROUP BY e.issuer_sha256 ORDER BY e.issuer_sha256 LIMIT $3
        ), facts AS (
            SELECT e.issuer_sha256, e.expires_at, e.prepared_at,
                (e.redeemed_at IS NULL AND g.state='authorized' AND g.expires_at>to_timestamp($4::bigint)
                 AND e.not_before<=to_timestamp($4::bigint) AND e.expires_at>to_timestamp($4::bigint)) AS waiting,
                (e.redeemed_at IS NOT NULL AND g.state='authorized' AND c.active AND c.node_id=e.node_id
                 AND n.state!='revoked' AND e.not_before<=to_timestamp($4::bigint) AND e.expires_at>to_timestamp($4::bigint) AND c.not_before<=to_timestamp($4::bigint) AND c.expires_at>to_timestamp($4::bigint)) AS live,
                LEAST(e.expires_at,c.expires_at) AS valid_until
            FROM page JOIN worker_enrollment_certificates e USING(issuer_sha256)
            JOIN worker_enrollment_grants g USING(request_id)
            LEFT JOIN worker_credentials c USING(certificate_sha256) LEFT JOIN nodes n ON n.node_id=c.node_id
            WHERE ($1::text IS NULL OR e.region_id=$1)
        )
        SELECT f.issuer_sha256,k.spki_sha256,count(*) AS retained,count(*) FILTER(WHERE waiting) AS prepared,
            count(*) FILTER(WHERE live) AS active,count(*) FILTER(WHERE live AND valid_until<=to_timestamp($4::bigint)+interval '1 hour') AS expiring,
            floor(extract(epoch FROM min(valid_until) FILTER(WHERE live)))::bigint AS next_expiry,
            floor(extract(epoch FROM max(prepared_at)))::bigint AS last_prepared
        FROM facts f LEFT JOIN worker_issuer_public_keys k USING(issuer_sha256)
        GROUP BY f.issuer_sha256,k.spki_sha256 ORDER BY f.issuer_sha256
    ").bind(region).bind(after).bind(i64::from(limit)+1).bind(at).fetch_all(&mut *tx).await?;
    let mut data = Vec::new();
    for row in rows.iter().take(usize::from(limit)) {
        data.push(json!({
            "issuer_sha256":row.try_get::<&str,_>("issuer_sha256")?,"signing_key_sha256":row.try_get::<Option<&str>,_>("spki_sha256")?,
            "retained_certificates":row.try_get::<i64,_>("retained")?.to_string(),"awaiting_exchange":row.try_get::<i64,_>("prepared")?.to_string(),
            "active_credentials":row.try_get::<i64,_>("active")?.to_string(),"expiring_within_hour":row.try_get::<i64,_>("expiring")?.to_string(),
            "next_expiry_unix":row.try_get::<Option<i64>,_>("next_expiry")?.map(|v|v.to_string()),
            "last_prepared_unix":row.try_get::<i64,_>("last_prepared")?.to_string()
        }));
    }
    let next = if rows.len() > usize::from(limit) {
        Some(position(
            &session,
            &scope,
            rows[usize::from(limit) - 1].try_get("issuer_sha256")?,
        )?)
    } else {
        None
    };
    audit::record_read(
        &mut tx,
        &session,
        &scope,
        "device_issuers.read",
        None,
        &request,
    )
    .await?;
    // Include time waiting on a busy audit sink in the final authorization decision.
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize(PERMISSION, &scope)?;
    tx.commit().await?;
    audit::response(
        json!({"scope":scope,"observed_unix":at.to_string(),"data":data,"next_cursor":next}),
        &request,
    )
}
