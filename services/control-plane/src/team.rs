//! Transactional team authority. Callers supply an authenticated user ID, never a request-body actor.
//! This slice has no public identity or invitation endpoint yet.
use orange_protocol::{request_digest, team::*, valid_id};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error("invalid team operation")]
    Invalid,
    #[error("current team or resource permission denied")]
    Forbidden,
    #[error("team resource not found")]
    NotFound,
    #[error("idempotency content conflict")]
    Conflict,
    #[error("policy or ACL version changed")]
    Version,
    #[error("last organization owner cannot be removed")]
    LastOwner,
    #[error("organization member limit reached")]
    Capacity,
    #[error("team database operation failed")]
    Database(#[from] sqlx::Error),
}
#[derive(Clone)]
pub struct TeamStore {
    pool: PgPool,
}
impl TeamStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub(crate) async fn list_organizations_in(
        tx: &mut Transaction<'_, Postgres>,
        actor: &str,
        after: &str,
        limit: u16,
    ) -> Result<orange_protocol::user::UserOrganizationList, TeamError> {
        identifiers(&[actor])?;
        if (!after.is_empty() && !valid_id(after)) || !(1..=100).contains(&limit) {
            return Err(TeamError::Invalid);
        }
        user(tx, actor, false).await?;
        let rows=sqlx::query("SELECT o.*,m.role,m.user_id AS member_user_id FROM organizations o JOIN memberships m USING(organization_id) JOIN owners w ON w.owner_id=o.organization_id WHERE m.user_id=$1 AND m.active AND w.state='active' AND o.organization_id>$2 ORDER BY o.organization_id LIMIT $3 FOR SHARE OF o,m,w")
            .bind(actor).bind(after).bind(i64::from(limit)+1).fetch_all(&mut **tx).await?;
        let more = rows.len() > usize::from(limit);
        let mut data = Vec::new();
        for row in rows.into_iter().take(usize::from(limit)) {
            data.push(user_organization(&row)?);
        }
        let next_cursor = if more {
            data.last().map(|v| v.organization_id.clone())
        } else {
            None
        };
        Ok(orange_protocol::user::UserOrganizationList {
            user_id: actor.into(),
            data,
            next_cursor,
        })
    }
    pub(crate) async fn organization_in(
        tx: &mut Transaction<'_, Postgres>,
        actor: &str,
        organization: &str,
    ) -> Result<orange_protocol::user::UserOrganization, TeamError> {
        identifiers(&[actor, organization])?;
        user(tx, actor, false).await?;
        let row=sqlx::query("SELECT o.*,m.role,m.user_id AS member_user_id FROM organizations o JOIN memberships m USING(organization_id) JOIN owners w ON w.owner_id=o.organization_id WHERE o.organization_id=$1 AND m.user_id=$2 AND m.active AND w.state='active' FOR SHARE OF o,m,w")
            .bind(organization).bind(actor).fetch_optional(&mut **tx).await?.ok_or(TeamError::NotFound)?;
        user_organization(&row)
    }
    pub async fn create_organization(
        &self,
        actor: &str,
        key: &str,
        request: &CreateOrganization,
    ) -> Result<OrganizationView, TeamError> {
        let mut tx = self.pool.begin().await?;
        let result = Self::create_organization_in(&mut tx, actor, key, request).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(crate) async fn create_organization_in(
        tx: &mut Transaction<'_, Postgres>,
        actor: &str,
        key: &str,
        request: &CreateOrganization,
    ) -> Result<OrganizationView, TeamError> {
        identifiers(&[actor, key])?;
        if request.name.trim() != request.name
            || request.name.is_empty()
            || request.name.chars().count() > 100
            || !(1..=20).contains(&request.member_limit)
        {
            return Err(TeamError::Invalid);
        }
        let digest = request_digest("team.create", request).map_err(|_| TeamError::Invalid)?;
        // Serialize creation keys without upgrading a user lock before taking organization locks.
        user(tx, actor, false).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("orange.team.create:{actor}"))
            .execute(&mut **tx)
            .await?;
        if let Some(value) =
            prior::<OrganizationView>(tx, actor, "personal", "team.create", key, &digest).await?
        {
            organization_row(tx, &value.organization_id, false).await?;
            membership(tx, &value.organization_id, actor).await?;
            return Ok(value);
        }
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "INSERT INTO organizations(organization_id,name,member_limit) VALUES($1,$2,$3)",
        )
        .bind(&id)
        .bind(&request.name)
        .bind(i32::from(request.member_limit))
        .execute(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'owner')")
            .bind(&id)
            .bind(actor)
            .execute(&mut **tx)
            .await?;
        let result = OrganizationView {
            organization_id: id.clone(),
            name: request.name.clone(),
            member_limit: request.member_limit,
            policy_version: 1,
        };
        record(tx, actor, "personal", "team.create", key, &digest, &result).await?;
        audit(tx, &id, actor, "organization.created", &id, &digest).await?;
        Ok(result)
    }
    pub async fn set_membership(
        &self,
        actor: &str,
        organization: &str,
        key: &str,
        request: &ChangeMembership,
    ) -> Result<MembershipChange, TeamError> {
        let mut tx = self.pool.begin().await?;
        let result = Self::set_membership_in(&mut tx, actor, organization, key, request).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(crate) async fn set_membership_in(
        tx: &mut Transaction<'_, Postgres>,
        actor: &str,
        organization: &str,
        key: &str,
        request: &ChangeMembership,
    ) -> Result<MembershipChange, TeamError> {
        identifiers(&[actor, organization, key, &request.user_id])?;
        let digest = request_digest("team.membership", request).map_err(|_| TeamError::Invalid)?;
        user(tx, actor, false).await?;
        let org = organization_row(tx, organization, true).await?;
        let authority = membership(tx, organization, actor).await?;
        if !matches!(authority, TeamRole::Owner | TeamRole::Admin) {
            return Err(TeamError::Forbidden);
        }
        // Replays still require current management authority, before reading any saved result.
        if let Some(value) = prior(tx, actor, organization, "team.membership", key, &digest).await?
        {
            return Ok(value);
        }
        if org.try_get::<i64, _>("policy_version")? != request.expected_policy_version {
            return Err(TeamError::Version);
        }
        if request.role.is_some() {
            user(tx, &request.user_id, false).await?;
        } else if sqlx::query("SELECT user_id FROM users WHERE user_id=$1 FOR SHARE")
            .bind(&request.user_id)
            .fetch_optional(&mut **tx)
            .await?
            .is_none()
        {
            return Err(TeamError::NotFound);
        }
        let previous = sqlx::query(
            "SELECT role,active FROM memberships WHERE organization_id=$1 AND user_id=$2",
        )
        .bind(organization)
        .bind(&request.user_id)
        .fetch_optional(&mut **tx)
        .await?;
        let current = previous
            .as_ref()
            .filter(|r| r.get::<bool, _>("active"))
            .map(|r| r.get::<String, _>("role"));
        if authority == TeamRole::Admin
            && (current
                .as_deref()
                .is_some_and(|r| matches!(r, "owner" | "admin"))
                || request
                    .role
                    .is_some_and(|r| matches!(r, TeamRole::Owner | TeamRole::Admin)))
        {
            return Err(TeamError::Forbidden);
        }
        if current.as_deref() == Some("owner") && request.role != Some(TeamRole::Owner) {
            let owners:i64=sqlx::query_scalar("SELECT count(*) FROM memberships m JOIN users u USING(user_id) WHERE m.organization_id=$1 AND m.role='owner' AND m.active AND u.active").bind(organization).fetch_one(&mut **tx).await?;
            if owners <= 1 {
                return Err(TeamError::LastOwner);
            }
        }
        if request.role.is_some() && current.is_none() {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM memberships WHERE organization_id=$1 AND active",
            )
            .bind(organization)
            .fetch_one(&mut **tx)
            .await?;
            if count >= i64::from(org.try_get::<i32, _>("member_limit")?) {
                return Err(TeamError::Capacity);
            }
        }
        if let Some(role) = request.role {
            sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,$3) ON CONFLICT(organization_id,user_id) DO UPDATE SET role=EXCLUDED.role,active=TRUE")
                .bind(organization).bind(&request.user_id).bind(role.as_str()).execute(&mut **tx).await?;
        } else {
            sqlx::query(
                "UPDATE memberships SET active=FALSE WHERE organization_id=$1 AND user_id=$2",
            )
            .bind(organization)
            .bind(&request.user_id)
            .execute(&mut **tx)
            .await?;
            // Rejoining must not restore old content grants, including a former creator's grants.
            sqlx::query("UPDATE resource_acl a SET revoked=TRUE FROM team_resources r WHERE a.resource_id=r.resource_id AND r.organization_id=$1 AND a.user_id=$2").bind(organization).bind(&request.user_id).execute(&mut **tx).await?;
        }
        let version:i64=sqlx::query_scalar("UPDATE organizations SET policy_version=policy_version+1 WHERE organization_id=$1 RETURNING policy_version").bind(organization).fetch_one(&mut **tx).await?;
        let result = MembershipChange {
            organization_id: organization.into(),
            user_id: request.user_id.clone(),
            role: request.role,
            policy_version: version,
        };
        record(
            tx,
            actor,
            organization,
            "team.membership",
            key,
            &digest,
            &result,
        )
        .await?;
        audit(
            tx,
            organization,
            actor,
            "membership.changed",
            &request.user_id,
            &digest,
        )
        .await?;
        Ok(result)
    }
    pub async fn register_resource(
        &self,
        actor: &str,
        organization: &str,
        key: &str,
        kind: &str,
    ) -> Result<ResourceView, TeamError> {
        identifiers(&[actor, organization, key])?;
        if !["thread", "run", "artifact", "memory", "skill"].contains(&kind) {
            return Err(TeamError::Invalid);
        }
        let digest = request_digest("resource.register", &kind).map_err(|_| TeamError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        user(&mut tx, actor, false).await?;
        organization_row(&mut tx, organization, true).await?;
        let role = membership(&mut tx, organization, actor).await?;
        if !role.can_create_content() {
            return Err(TeamError::Forbidden);
        }
        if let Some(value) = prior::<ResourceView>(
            &mut tx,
            actor,
            organization,
            "resource.register",
            key,
            &digest,
        )
        .await?
        {
            resource_row(&mut tx, organization, &value.resource_id, false).await?;
            resource_permission(
                &mut tx,
                actor,
                &value.resource_id,
                role,
                ResourceAccess::Write,
            )
            .await?;
            tx.commit().await?;
            return Ok(value);
        }
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO team_resources(resource_id,organization_id,creator_id,kind) VALUES($1,$2,$3,$4)").bind(&id).bind(organization).bind(actor).bind(kind).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO resource_acl(resource_id,user_id,access) VALUES($1,$2,'write')")
            .bind(&id)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        let result = ResourceView {
            resource_id: id.clone(),
            organization_id: organization.into(),
            creator_id: actor.into(),
            kind: kind.into(),
            acl_version: 1,
        };
        record(
            &mut tx,
            actor,
            organization,
            "resource.register",
            key,
            &digest,
            &result,
        )
        .await?;
        audit(
            &mut tx,
            organization,
            actor,
            "resource.registered",
            &id,
            &digest,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
    /// Returns metadata after current authorization. It is not a reusable content-delivery capability.
    pub async fn resource(
        &self,
        actor: &str,
        organization: &str,
        resource: &str,
        access: ResourceAccess,
    ) -> Result<ResourceView, TeamError> {
        identifiers(&[actor, organization, resource])?;
        let mut tx = self.pool.begin().await?;
        user(&mut tx, actor, false).await?;
        organization_row(&mut tx, organization, false).await?;
        let role = membership(&mut tx, organization, actor).await?;
        let row = resource_row(&mut tx, organization, resource, false).await?;
        resource_permission(&mut tx, actor, resource, role, access).await?;
        let view = resource_view(&row)?;
        tx.commit().await?;
        Ok(view)
    }
    pub async fn set_acl(
        &self,
        actor: &str,
        organization: &str,
        resource: &str,
        key: &str,
        request: &ChangeResourceAcl,
    ) -> Result<ResourceView, TeamError> {
        identifiers(&[actor, organization, resource, key, &request.user_id])?;
        let digest =
            request_digest("resource.acl", &(resource, request)).map_err(|_| TeamError::Invalid)?;
        let mut tx = self.pool.begin().await?;
        user(&mut tx, actor, false).await?;
        organization_row(&mut tx, organization, true).await?;
        let role = membership(&mut tx, organization, actor).await?;
        let row = resource_row(&mut tx, organization, resource, true).await?;
        resource_permission(&mut tx, actor, resource, role, ResourceAccess::Write).await?;
        if row.try_get::<String, _>("creator_id")? != actor {
            return Err(TeamError::Forbidden);
        }
        if let Some(value) =
            prior(&mut tx, actor, organization, "resource.acl", key, &digest).await?
        {
            tx.commit().await?;
            return Ok(value);
        }
        if row.try_get::<i64, _>("acl_version")? != request.expected_acl_version {
            return Err(TeamError::Version);
        }
        if let Some(access) = request.access {
            user(&mut tx, &request.user_id, false).await?;
            membership(&mut tx, organization, &request.user_id).await?;
            sqlx::query("INSERT INTO resource_acl(resource_id,user_id,access) VALUES($1,$2,$3) ON CONFLICT(resource_id,user_id) DO UPDATE SET access=EXCLUDED.access,revoked=FALSE").bind(resource).bind(&request.user_id).bind(access.as_str()).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE resource_acl SET revoked=TRUE WHERE resource_id=$1 AND user_id=$2")
                .bind(resource)
                .bind(&request.user_id)
                .execute(&mut *tx)
                .await?;
        }
        let row = sqlx::query(
            "UPDATE team_resources SET acl_version=acl_version+1 WHERE resource_id=$1 RETURNING *",
        )
        .bind(resource)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE organizations SET policy_version=policy_version+1 WHERE organization_id=$1",
        )
        .bind(organization)
        .execute(&mut *tx)
        .await?;
        let result = resource_view(&row)?;
        record(
            &mut tx,
            actor,
            organization,
            "resource.acl",
            key,
            &digest,
            &result,
        )
        .await?;
        audit(
            &mut tx,
            organization,
            actor,
            "resource.acl_changed",
            resource,
            &digest,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}
fn identifiers(values: &[&str]) -> Result<(), TeamError> {
    if values.iter().all(|s| valid_id(s)) {
        Ok(())
    } else {
        Err(TeamError::Invalid)
    }
}
fn user_organization(
    row: &sqlx::postgres::PgRow,
) -> Result<orange_protocol::user::UserOrganization, TeamError> {
    Ok(orange_protocol::user::UserOrganization {
        member_user_id: row.try_get("member_user_id")?,
        organization_id: row.try_get("organization_id")?,
        name: row.try_get("name")?,
        role: row
            .try_get::<String, _>("role")?
            .parse()
            .map_err(|_| TeamError::Forbidden)?,
        member_limit: u16::try_from(row.try_get::<i32, _>("member_limit")?)
            .map_err(|_| TeamError::Invalid)?,
        policy_version: row.try_get::<i64, _>("policy_version")?.to_string(),
    })
}
async fn user(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    exclusive: bool,
) -> Result<(), TeamError> {
    let sql = if exclusive {
        "SELECT user_id FROM users WHERE user_id=$1 AND active FOR UPDATE"
    } else {
        "SELECT user_id FROM users WHERE user_id=$1 AND active FOR SHARE"
    };
    if sqlx::query(sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .is_none()
    {
        return Err(TeamError::Forbidden);
    }
    Ok(())
}
pub(crate) async fn organization_row(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    exclusive: bool,
) -> Result<sqlx::postgres::PgRow, TeamError> {
    let sql = if exclusive {
        "SELECT o.* FROM organizations o JOIN owners w ON w.owner_id=o.organization_id WHERE o.organization_id=$1 AND w.state='active' FOR UPDATE OF o FOR SHARE OF w"
    } else {
        "SELECT o.* FROM organizations o JOIN owners w ON w.owner_id=o.organization_id WHERE o.organization_id=$1 AND w.state='active' FOR SHARE OF o,w"
    };
    sqlx::query(sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(TeamError::Forbidden)
}
pub(crate) async fn membership(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    user: &str,
) -> Result<TeamRole, TeamError> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active FOR SHARE",
    )
    .bind(org)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    role.ok_or(TeamError::Forbidden)?
        .parse()
        .map_err(|_| TeamError::Forbidden)
}
async fn resource_row(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    exclusive: bool,
) -> Result<sqlx::postgres::PgRow, TeamError> {
    let sql = if exclusive {
        "SELECT * FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND NOT deleted FOR UPDATE"
    } else {
        "SELECT * FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND NOT deleted FOR SHARE"
    };
    sqlx::query(sql)
        .bind(id)
        .bind(org)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(TeamError::NotFound)
}
pub(crate) async fn resource_permission(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    resource: &str,
    role: TeamRole,
    access: ResourceAccess,
) -> Result<(), TeamError> {
    if !role.can_read_content() || (access == ResourceAccess::Write && !role.can_create_content()) {
        return Err(TeamError::Forbidden);
    }
    let grant:Option<String>=sqlx::query_scalar("SELECT access FROM resource_acl WHERE resource_id=$1 AND user_id=$2 AND NOT revoked FOR SHARE").bind(resource).bind(actor).fetch_optional(&mut **tx).await?;
    if !grant
        .as_deref()
        .is_some_and(|g| g == "write" || (g == "read" && access == ResourceAccess::Read))
    {
        return Err(TeamError::Forbidden);
    }
    Ok(())
}
fn resource_view(row: &sqlx::postgres::PgRow) -> Result<ResourceView, TeamError> {
    Ok(ResourceView {
        resource_id: row.try_get("resource_id")?,
        organization_id: row.try_get("organization_id")?,
        creator_id: row.try_get("creator_id")?,
        kind: row.try_get("kind")?,
        acl_version: row.try_get("acl_version")?,
    })
}
pub(crate) async fn prior<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    scope: &str,
    operation: &str,
    key: &str,
    digest: &str,
) -> Result<Option<T>, TeamError> {
    let row=sqlx::query("SELECT digest,result::text AS result FROM team_request_keys WHERE actor_id=$1 AND scope=$2 AND operation=$3 AND key=$4").bind(actor).bind(scope).bind(operation).bind(key).fetch_optional(&mut **tx).await?;
    if let Some(row) = row {
        if row.try_get::<String, _>("digest")? != digest {
            return Err(TeamError::Conflict);
        }
        return Ok(Some(
            serde_json::from_str(&row.try_get::<String, _>("result")?)
                .map_err(|_| TeamError::Conflict)?,
        ));
    }
    Ok(None)
}
pub(crate) async fn record<T: Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    scope: &str,
    operation: &str,
    key: &str,
    digest: &str,
    result: &T,
) -> Result<(), TeamError> {
    sqlx::query("INSERT INTO team_request_keys(actor_id,scope,operation,key,digest,result) VALUES($1,$2,$3,$4,$5,$6::jsonb)").bind(actor).bind(scope).bind(operation).bind(key).bind(digest).bind(serde_json::to_string(result).map_err(|_|TeamError::Invalid)?).execute(&mut **tx).await?;
    Ok(())
}
pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    actor: &str,
    kind: &str,
    target: &str,
    digest: &str,
) -> Result<(), TeamError> {
    sqlx::query("INSERT INTO team_audit(event_id,organization_id,actor_id,kind,target_id,payload_digest) VALUES($1,$2,$3,$4,$5,$6)").bind(Uuid::new_v4().to_string()).bind(org).bind(actor).bind(kind).bind(target).bind(digest).execute(&mut **tx).await?;
    Ok(())
}
