//! An authenticated creator can inspect or change the ACL of one live team resource.
use super::*;
use orange_protocol::team::{ChangeResourceAcl, ResourceAccess};
use serde_json::{Value, json};
use sqlx::Row;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    user_id: String,
    #[serde(deserialize_with = "explicit_access")]
    access: Option<ResourceAccess>,
    expected_acl_version: String,
}
fn explicit_access<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ResourceAccess>, D::Error> {
    Option::<ResourceAccess>::deserialize(d)
}

pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    Path((org, resource)): Path<(String, String)>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Json<Value>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    if !valid_id(&org) || !valid_id(&resource) || query.as_deref().is_some_and(|q| !q.is_empty()) {
        return Err(TeamError::Invalid.into());
    }
    let actor = guard.view().user_id.clone();
    let tx = guard.transaction();
    crate::team::organization_row(tx, &org, false).await?;
    let role = crate::team::membership(tx, &org, &actor).await?;
    let row = crate::team::resource_row(tx, &org, &resource, false).await?;
    if row.try_get::<&str, _>("creator_id")? != actor {
        return Err(TeamError::Forbidden.into());
    }
    crate::team::resource_permission(tx, &actor, &resource, role, ResourceAccess::Write).await?;
    // Organizations have at most 100 active members. Historical grants to removed
    // users are intentionally not returned as current recipients.
    let grants:Vec<(String,String)>=sqlx::query_as("SELECT g.user_id,g.access FROM resource_acl g
        JOIN memberships m ON m.user_id=g.user_id AND m.organization_id=$2
        JOIN users u ON u.user_id=g.user_id WHERE g.resource_id=$1 AND NOT g.revoked AND m.active AND u.active ORDER BY g.user_id LIMIT 101")
        .bind(&resource).bind(&org).fetch_all(&mut **tx).await?;
    if grants.len() > 100 {
        return Err(TeamError::Capacity.into());
    }
    let version = row.try_get::<i64, _>("acl_version")?.to_string();
    guard.finish().await?;
    Ok(Json(
        json!({"organization_id":org,"resource_id":resource,"creator_id":actor,"acl_version":version,
        "data":grants.into_iter().map(|(user_id,access)|json!({"user_id":user_id,"access":access})).collect::<Vec<_>>()}),
    ))
}

pub(super) async fn change(
    State(identity): State<Arc<UserIdentity>>,
    Path((org, resource)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Change>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let (mut guard, key) = super::team_management::write(&identity, &headers, None).await?;
    let Json(body) = body.map_err(|_| TeamError::Invalid)?;
    let expected = body
        .expected_acl_version
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == body.expected_acl_version)
        .ok_or(TeamError::Invalid)?;
    let actor = guard.view().user_id.clone();
    let user = body.user_id;
    let request = ChangeResourceAcl {
        user_id: user.clone(),
        access: body.access,
        expected_acl_version: expected,
    };
    let view =
        TeamStore::set_acl_in(guard.transaction(), &actor, &org, &resource, &key, &request).await?;
    guard.finish().await?;
    Ok(Json(
        json!({"organization_id":org,"resource_id":view.resource_id,"creator_id":view.creator_id,
                  "acl_version":view.acl_version.to_string(),"user_id":user,"access":request.access}),
    ))
}
