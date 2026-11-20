use super::*;
use orange_protocol::team::{ResourceAccess, TeamRole};
/// Current user session, owner and organization are locked only during database
/// decisions. Object I/O always happens after committing/releasing this scope.
pub(crate) async fn scope(
    tx: &mut Tx<'_>,
    actor: &ArtifactActor,
    owner: &str,
    write: bool,
) -> Result<Option<TeamRole>, ArtifactError> {
    if !valid_id(owner) || write && !actor.write {
        return Err(ArtifactError::Forbidden);
    }
    let personal:Option<String>=sqlx::query_scalar("SELECT u.personal_owner_id FROM user_sessions s JOIN users u USING(user_id) JOIN owners o ON o.owner_id=u.personal_owner_id WHERE s.session_id=$1 AND s.user_id=$2 AND NOT s.revoked AND s.idle_expires_at>floor(extract(epoch FROM clock_timestamp()))::bigint AND s.absolute_expires_at>floor(extract(epoch FROM clock_timestamp()))::bigint AND u.active AND o.state='active' FOR SHARE OF s,u,o").bind(&actor.session).bind(&actor.user).fetch_optional(&mut **tx).await?;
    personal.ok_or(ArtifactError::Forbidden)?;
    user_scope(tx, &actor.user, owner, write).await
}
pub(crate) async fn user_scope(
    tx: &mut Tx<'_>,
    user: &str,
    owner: &str,
    write: bool,
) -> Result<Option<TeamRole>, ArtifactError> {
    let personal:Option<String>=sqlx::query_scalar("SELECT u.personal_owner_id FROM users u JOIN owners o ON o.owner_id=u.personal_owner_id WHERE u.user_id=$1 AND u.active AND o.state='active' FOR SHARE OF u,o").bind(user).fetch_optional(&mut **tx).await?;
    let personal = personal.ok_or(ArtifactError::Forbidden)?;
    if personal == owner {
        return Ok(None);
    }
    crate::team::organization_row(tx, owner, false)
        .await
        .map_err(|_| ArtifactError::Forbidden)?;
    let role = crate::team::membership(tx, owner, user)
        .await
        .map_err(|_| ArtifactError::Forbidden)?;
    if !role.can_read_content() || write && !role.can_create_content() {
        return Err(ArtifactError::Forbidden);
    }
    Ok(Some(role))
}
pub(super) async fn resource(
    tx: &mut Tx<'_>,
    actor: &ArtifactActor,
    owner: &str,
    row: &sqlx::postgres::PgRow,
    write: bool,
    deleted_metadata: bool,
) -> Result<(), ArtifactError> {
    if row.try_get::<String, _>("owner_id")? != owner {
        return Err(ArtifactError::NotFound);
    }
    let role = scope(tx, actor, owner, write).await?;
    resource_role(tx, &actor.user, owner, row, role, write, deleted_metadata).await
}
pub(super) async fn resource_role(
    tx: &mut Tx<'_>,
    user: &str,
    owner: &str,
    row: &sqlx::postgres::PgRow,
    role: Option<TeamRole>,
    write: bool,
    deleted_metadata: bool,
) -> Result<(), ArtifactError> {
    if row.try_get::<String, _>("owner_id")? != owner {
        return Err(ArtifactError::NotFound);
    }
    if !deleted_metadata && !write {
        crate::cloud_model::output_access(tx, row.try_get("artifact_id")?).await?;
    }
    let resource: Option<String> = row.try_get("resource_id")?;
    match (role, resource) {
        (None, None) => Ok(()),
        (Some(role), Some(id)) => {
            let exists:Option<String>=sqlx::query_scalar("SELECT resource_id FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND kind='artifact' AND ($3 OR NOT deleted) FOR SHARE").bind(&id).bind(owner).bind(deleted_metadata).fetch_optional(&mut **tx).await?;
            if exists.is_none() {
                return Err(ArtifactError::Forbidden);
            }
            crate::team::resource_permission(
                tx,
                user,
                &id,
                role,
                if write {
                    ResourceAccess::Write
                } else {
                    ResourceAccess::Read
                },
            )
            .await
            .map_err(|_| ArtifactError::Forbidden)
        }
        _ => Err(ArtifactError::Integrity),
    }?;
    if !write {
        let job: Option<String> =
            sqlx::query_scalar("SELECT job_id FROM worker_output_uploads WHERE artifact_id=$1")
                .bind(row.try_get::<String, _>("artifact_id")?)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some(job) = job {
            super::inputs::source_read_in(tx, &job, user).await?;
        }
    }
    Ok(())
}
pub(crate) async fn finish(mut tx: Tx<'_>, actor: &ArtifactActor) -> Result<(), ArtifactError> {
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_sessions WHERE session_id=$1 AND user_id=$2 AND NOT revoked AND idle_expires_at>floor(extract(epoch FROM clock_timestamp()))::bigint AND absolute_expires_at>floor(extract(epoch FROM clock_timestamp()))::bigint)").bind(&actor.session).bind(&actor.user).fetch_one(&mut *tx).await?;
    if !live {
        return Err(ArtifactError::Forbidden);
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn stamp(
    tx: &mut Tx<'_>,
    row: &sqlx::postgres::PgRow,
) -> Result<ReadStamp, ArtifactError> {
    let resource: Option<String> = row.try_get("resource_id")?;
    if let Some(resource) = resource {
        let (organization,acl):(i64,i64)=sqlx::query_as("SELECT o.policy_version,r.acl_version FROM team_resources r JOIN organizations o ON o.organization_id=r.organization_id WHERE r.resource_id=$1 FOR SHARE OF o,r").bind(resource).fetch_one(&mut **tx).await?;
        Ok(ReadStamp {
            organization: Some(organization),
            acl: Some(acl),
        })
    } else {
        Ok(ReadStamp {
            organization: None,
            acl: None,
        })
    }
}
