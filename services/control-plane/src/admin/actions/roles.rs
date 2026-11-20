//! Explicit, expiring role bindings for already enrolled administrators.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub(super) principal_id: String,
    role: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    expires_at: Option<String>,
}
impl Binding {
    pub(super) fn validate(&self, r: &ActionRequest) -> Result<(), AdminError> {
        if !matches!(
            r.action_type.as_str(),
            "admin.role_grant" | "admin.role_revoke"
        ) || r.scope.environment != "dev"
            || !id(&self.principal_id)
            || !role_permissions().contains_key(&self.role)
            || !orange_protocol::valid_digest(&r.target_version)
            || (r.action_type == "admin.role_grant") != self.expires_at.is_some()
        {
            return Err(AdminError::Invalid);
        }
        if let Some(at) = &self.expires_at {
            support::seconds(at)?;
        }
        Ok(())
    }
    pub(super) fn authorize_participant(&self, who: &str, deadline: i64) -> Result<(), AdminError> {
        if self.principal_id == who {
            return Err(AdminError::Forbidden);
        }
        if let Some(at) = &self.expires_at
            && support::seconds(at)? > deadline
        {
            return Err(AdminError::Forbidden);
        }
        Ok(())
    }
}
async fn projection(
    tx: &mut Tx<'_>,
    principal: &str,
    role: &str,
    scope: &Scope,
) -> Result<Value, AdminError> {
    if !id(principal) || scope.environment != "dev" || !role_permissions().contains_key(role) {
        return Err(AdminError::Invalid);
    }
    let policy: i64 = sqlx::query_scalar(
        "SELECT policy_version FROM admin_principals WHERE principal_id=$1 AND active FOR SHARE",
    )
    .bind(principal)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AdminError::NotFound)?;
    let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM admin_role_revisions WHERE principal_id=$1 AND role=$2
        AND environment=$3 AND scope_kind=$4 AND scope_id=$5 FOR SHARE",
    )
    .bind(principal)
    .bind(role)
    .bind(&scope.environment)
    .bind(&scope.kind)
    .bind(&scope.scope_id)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(0);
    let grant: Option<(bool, i64)> = sqlx::query_as(
        "SELECT revoked,expires_at FROM admin_grants WHERE principal_id=$1 AND role=$2
        AND environment=$3 AND scope_kind=$4 AND scope_id=$5 FOR SHARE",
    )
    .bind(principal)
    .bind(role)
    .bind(&scope.environment)
    .bind(&scope.kind)
    .bind(&scope.scope_id)
    .fetch_optional(&mut **tx)
    .await?;
    let time = now(tx).await?;
    let state = match grant {
        None => "absent",
        Some((true, _)) => "revoked",
        Some((false, until)) if until <= time => "expired",
        _ => "active",
    };
    let hash = digest(
        "admin.role-binding.v1",
        &(principal, role, scope, policy, revision, grant, state),
    )?;
    let mut result = json!({"principal_id":principal,"role":role,"scope":scope,"state":state,"revision":revision.to_string(),"target_version":hash});
    if let Some((_, until)) = grant {
        result["expires_at"] = json!(timestamp(until)?);
    }
    Ok(result)
}
pub(super) async fn target(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    b: &Binding,
) -> Result<(), AdminError> {
    let row = projection(tx, &b.principal_id, &b.role, &r.scope).await?;
    if row["target_version"] != r.target_version {
        return Err(AdminError::Precondition);
    }
    match (&b.expires_at, row["state"].as_str()) {
        (None, Some("active")) => Ok(()),
        (Some(at), _) => {
            let until = support::seconds(at)?;
            if until <= now(tx).await?
                || (row["state"] == "active"
                    && until <= support::seconds(text(&row, "expires_at")?)?)
            {
                return Err(AdminError::Conflict);
            }
            Ok(())
        }
        _ => Err(AdminError::Conflict),
    }
}
/// Bind the actor's approval to the revisions of the exact scoped roles used
/// for it. Removing and recreating a binding cannot validate an earlier approval.
pub(super) async fn witness(
    tx: &mut Tx<'_>,
    action: &str,
    principal: &str,
    r: &ActionRequest,
    phase: &str,
    record: bool,
) -> Result<(), AdminError> {
    let permission = if phase == "maker" {
        r.permission()?
    } else {
        r.checker_permission().ok_or(AdminError::Invalid)?
    };
    let registry = role_permissions();
    let roles: Vec<&str> = registry
        .iter()
        .filter(|(_, perms)| perms.contains(permission))
        .map(|(role, _)| role.as_str())
        .collect();
    let rows:Vec<(String,i64,i64)>=sqlx::query_as("SELECT g.role,v.revision,g.expires_at FROM admin_grants g JOIN admin_role_revisions v
        USING(principal_id,role,environment,scope_kind,scope_id)
        WHERE g.principal_id=$1 AND g.environment=$2 AND g.scope_kind=$3 AND g.scope_id=$4 AND g.role=ANY($5)
          AND NOT g.revoked AND g.expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint ORDER BY g.role FOR SHARE OF g,v")
        .bind(principal).bind(&r.scope.environment).bind(&r.scope.kind).bind(&r.scope.scope_id).bind(roles)
        .fetch_all(&mut **tx).await?;
    if rows.is_empty() {
        return Err(AdminError::Forbidden);
    }
    let policy: i64 = sqlx::query_scalar(
        "SELECT policy_version FROM admin_principals WHERE principal_id=$1 AND active FOR SHARE",
    )
    .bind(principal)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AdminError::Forbidden)?;
    let value = digest(
        "admin.role-authority.v1",
        &(principal, &r.scope, phase, permission, policy, rows),
    )?;
    if record {
        sqlx::query("INSERT INTO admin_role_witnesses(action_id,phase,principal_id,authority_digest) VALUES($1,$2,$3,$4)")
            .bind(action).bind(phase).bind(principal).bind(value).execute(&mut **tx).await?;
    } else {
        let known: Option<String> = sqlx::query_scalar(
            "SELECT authority_digest FROM admin_role_witnesses
            WHERE action_id=$1 AND phase=$2 AND principal_id=$3",
        )
        .bind(action)
        .bind(phase)
        .bind(principal)
        .fetch_optional(&mut **tx)
        .await?;
        if known.as_deref() != Some(&value) {
            return Err(AdminError::Forbidden);
        }
    }
    Ok(())
}
pub(super) async fn apply(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    b: &Binding,
) -> Result<(), AdminError> {
    if let Some(at) = &b.expires_at {
        sqlx::query(
            "INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at)
            VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(principal_id,role,environment,scope_kind,scope_id)
            DO UPDATE SET expires_at=EXCLUDED.expires_at,revoked=false",
        )
        .bind(&b.principal_id)
        .bind(&b.role)
        .bind(&r.scope.environment)
        .bind(&r.scope.kind)
        .bind(&r.scope.scope_id)
        .bind(support::seconds(at)?)
        .execute(&mut **tx)
        .await?;
    } else {
        let result=sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND role=$2 AND environment=$3
            AND scope_kind=$4 AND scope_id=$5 AND NOT revoked")
            .bind(&b.principal_id).bind(&b.role).bind(&r.scope.environment).bind(&r.scope.kind).bind(&r.scope.scope_id)
            .execute(&mut **tx).await?;
        if result.rows_affected() != 1 {
            return Err(AdminError::Conflict);
        }
    }
    sqlx::query(
        "UPDATE admin_principals SET policy_version=policy_version+1 WHERE principal_id=$1",
    )
    .bind(&b.principal_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    environment: String,
    scope_kind: String,
    scope_id: String,
}
pub async fn review(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path((principal, role)): Path<(String, String)>,
    query: Result<axum::extract::Query<Query>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let q = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    if !matches!(
        scope.kind.as_str(),
        "tenant" | "platform" | "region" | "resource"
    ) || !id(&scope.scope_id)
        || (scope.kind == "platform" && scope.scope_id != "platform")
    {
        return Err(AdminError::Invalid);
    }
    let mut tx = begin(&identity).await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("admins.roles.read", &scope)?;
    let row = projection(&mut tx, &principal, &role, &scope).await?;
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at)
        VALUES($1,$2,'admin.role_binding_reviewed',$3,$4,$5,$6,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&session.view.principal_id)
        .bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(principal).execute(&mut *tx).await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("admins.roles.read", &scope)?;
    tx.commit().await?;
    Ok(Json(row))
}
