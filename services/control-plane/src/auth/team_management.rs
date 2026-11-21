//! Membership changes and addressed invitations, authorized in the same transaction as the user session.
use super::*;
use crate::auth::users::UserSessionGuard;
use orange_protocol::team::{ChangeMembership, TeamRole};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Issue {
    invitation_id: String,
    recipient_id: String,
    role: TeamRole,
    ttl_seconds: u32,
    expected_policy_version: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Accept {
    accept: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Empty {}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    after: Option<String>,
    limit: Option<u16>,
}
fn positive(s: &str) -> Result<i64, UserApiError> {
    s.parse::<i64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == s)
        .ok_or_else(|| TeamError::Invalid.into())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    user_id: String,
    #[serde(deserialize_with = "required_role")]
    role: Option<TeamRole>,
    expected_policy_version: String,
}
fn required_role<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<TeamRole>, D::Error> {
    Option::<TeamRole>::deserialize(d)
}
fn invite_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|v| v.to_string() == s && v.get_version_num() == 4)
}
pub(super) async fn write(
    identity: &UserIdentity,
    h: &HeaderMap,
    key: Option<&str>,
) -> Result<(UserSessionGuard, String), UserApiError> {
    let g = identity.authenticate(h).await?;
    check_write(h, &identity.config.user_origin, &g.view().csrf_token)?;
    let k = h
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| valid_id(s))
        .ok_or(AuthError::Invalid)?;
    if key.is_some_and(|expected| expected != k) {
        return Err(AuthError::Invalid.into());
    }
    Ok((g, k.to_owned()))
}
async fn manager(
    tx: &mut Tx<'_>,
    org: &str,
    user: &str,
    exclusive: bool,
) -> Result<(TeamRole, i64), UserApiError> {
    let o = crate::team::organization_row(tx, org, exclusive).await?;
    let role = crate::team::membership(tx, org, user).await?;
    if !matches!(role, TeamRole::Admin | TeamRole::Owner) {
        return Err(TeamError::Forbidden.into());
    }
    Ok((role, o.try_get("policy_version")?))
}
fn rank_can_grant(issuer: TeamRole, role: TeamRole) -> bool {
    issuer == TeamRole::Owner
        || issuer == TeamRole::Admin && !matches!(role, TeamRole::Owner | TeamRole::Admin)
}
const SELECT_INVITE: &str = "SELECT i.*,o.name,floor(extract(epoch FROM i.expires_at))::bigint AS expiry FROM organization_invitations i JOIN organizations o USING(organization_id) WHERE i.invitation_id=$1";
fn view(r: &sqlx::postgres::PgRow) -> Result<Value, UserApiError> {
    Ok(
        json!({"invitation_id":r.try_get::<&str,_>("invitation_id")?,"organization_id":r.try_get::<&str,_>("organization_id")?,"organization_name":r.try_get::<&str,_>("name")?,
        "issuer_id":r.try_get::<&str,_>("issuer_id")?,"recipient_id":r.try_get::<&str,_>("recipient_id")?,"role":r.try_get::<&str,_>("role")?,
        "state":r.try_get::<&str,_>("state")?,"expires_at_unix":r.try_get::<i64,_>("expiry")?.to_string(),"accepted_policy_version":r.try_get::<Option<i64>,_>("accepted_version")?.map(|x|x.to_string())}),
    )
}
async fn current(tx: &mut Tx<'_>, id: &str) -> Result<Value, UserApiError> {
    let row = sqlx::query(SELECT_INVITE)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(TeamError::NotFound)?;
    view(&row)
}
async fn audit(
    tx: &mut Tx<'_>,
    org: &str,
    user: &str,
    id: &str,
    kind: &str,
) -> Result<(), UserApiError> {
    let digest =
        orange_protocol::request_digest("organization.invite.event.v1", &(org, user, id, kind))
            .map_err(|_| TeamError::Invalid)?;
    sqlx::query("INSERT INTO team_audit(event_id,organization_id,actor_id,kind,target_id,payload_digest) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(org).bind(user).bind(kind).bind(id).bind(digest).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn members(
    State(identity): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(&h).await?;
    let user = g.view().user_id.clone();
    let (_, version) = manager(g.transaction(), &org, &user, false).await?;
    let rows:Vec<(String,String)>=sqlx::query_as("SELECT m.user_id,m.role FROM memberships m JOIN users u USING(user_id) WHERE m.organization_id=$1 AND m.active AND u.active ORDER BY m.user_id LIMIT 101")
        .bind(&org).fetch_all(&mut **g.transaction()).await?;
    if rows.len() > 100 {
        return Err(TeamError::Capacity.into());
    }
    g.finish().await?;
    Ok(Json(
        json!({"organization_id":org,"policy_version":version.to_string(),"data":rows.into_iter().map(|(user,role)|json!({"user_id":user,"role":role})).collect::<Vec<_>>()}),
    ))
}
pub(super) async fn change(
    State(identity): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    body: Result<Json<Change>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let (mut g, key) = write(&identity, &h, None).await?;
    let Json(r) = body.map_err(|_| TeamError::Invalid)?;
    let request = ChangeMembership {
        user_id: r.user_id,
        role: r.role,
        expected_policy_version: positive(&r.expected_policy_version)?,
    };
    let user = g.view().user_id.clone();
    manager(g.transaction(), &org, &user, true).await?;
    let key = orange_protocol::request_digest("user.organization.change-key.v1", &key)
        .map_err(|_| TeamError::Invalid)?;
    // Addition is a separate consented action. Read the prior key on replay of a successful removal.
    let was_active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active)").bind(&org).bind(&request.user_id).fetch_one(&mut **g.transaction()).await?;
    if request.role.is_some() && !was_active {
        return Err(TeamError::NotFound.into());
    }
    let changed =
        TeamStore::set_membership_in(g.transaction(), &user, &org, &key, &request).await?;
    g.finish().await?;
    Ok(Json(
        json!({"organization_id":changed.organization_id,"user_id":changed.user_id,"role":changed.role,"policy_version":changed.policy_version.to_string()}),
    ))
}
pub(super) async fn issue(
    State(identity): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    body: Result<Json<Issue>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(r) = body.map_err(|_| TeamError::Invalid)?;
    let (mut g, _) = write(&identity, &h, Some(&r.invitation_id)).await?;
    let user = g.view().user_id.clone();
    if !invite_id(&r.invitation_id)
        || !valid_id(&r.recipient_id)
        || r.recipient_id == user
        || !(1..=604800).contains(&r.ttl_seconds)
    {
        return Err(TeamError::Invalid.into());
    }
    let (role, version) = manager(g.transaction(), &org, &user, true).await?;
    if !rank_can_grant(role, r.role) {
        return Err(TeamError::Forbidden.into());
    }
    let digest = orange_protocol::request_digest("user.team-invitation.v1", &(&org, &user, &r))
        .map_err(|_| TeamError::Invalid)?;
    sqlx::query("UPDATE organization_invitations SET state='expired' WHERE organization_id=$1 AND recipient_id=$2 AND state='pending' AND expires_at<=clock_timestamp()").bind(&org).bind(&r.recipient_id).execute(&mut **g.transaction()).await?;
    let prior:Option<String>=sqlx::query_scalar("SELECT request_digest FROM organization_invitations WHERE invitation_id=$1 AND organization_id=$2 AND issuer_id=$3").bind(&r.invitation_id).bind(&org).bind(&user).fetch_optional(&mut **g.transaction()).await?;
    if let Some(prior) = prior {
        if prior != digest {
            return Err(TeamError::Conflict.into());
        }
    } else {
        if version != positive(&r.expected_policy_version)? {
            return Err(TeamError::Version.into());
        }
        sqlx::query("SELECT user_id FROM users WHERE user_id=$1 AND active FOR SHARE")
            .bind(&r.recipient_id)
            .fetch_optional(&mut **g.transaction())
            .await?
            .ok_or(TeamError::NotFound)?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active)").bind(&org).bind(&r.recipient_id).fetch_one(&mut **g.transaction()).await?;
        if active {
            return Err(TeamError::Conflict.into());
        }
        let created=sqlx::query("INSERT INTO organization_invitations(invitation_id,organization_id,issuer_id,recipient_id,role,request_digest,state,expires_at)
            VALUES($1,$2,$3,$4,$5,$6,'pending',clock_timestamp()+make_interval(secs=>$7)) ON CONFLICT DO NOTHING")
           .bind(&r.invitation_id).bind(&org).bind(&user).bind(&r.recipient_id).bind(r.role.as_str()).bind(&digest).bind(f64::from(r.ttl_seconds)).execute(&mut **g.transaction()).await?.rows_affected();
        if created != 1 {
            return Err(TeamError::Conflict.into());
        }
        audit(
            g.transaction(),
            &org,
            &user,
            &r.invitation_id,
            "invitation.issued",
        )
        .await?;
    }
    let v = current(g.transaction(), &r.invitation_id).await?;
    g.finish().await?;
    Ok(Json(v))
}
async fn list(
    identity: &UserIdentity,
    h: &HeaderMap,
    org: Option<String>,
    q: Page,
) -> Result<Json<Value>, UserApiError> {
    let mut g = identity.authenticate(h).await?;
    let user = g.view().user_id.clone();
    let limit = q.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) || q.after.as_deref().is_some_and(|id| !invite_id(id)) {
        return Err(TeamError::Invalid.into());
    }
    if let Some(id) = &org {
        manager(g.transaction(), id, &user, false).await?;
    }
    let rows=sqlx::query("SELECT i.invitation_id,i.organization_id,i.issuer_id,i.recipient_id,i.role,i.accepted_version,o.name,floor(extract(epoch FROM expires_at))::bigint AS expiry,
        CASE WHEN i.state='pending' AND i.expires_at<=clock_timestamp() THEN 'expired' ELSE i.state END AS state
        FROM organization_invitations i JOIN organizations o USING(organization_id) JOIN owners w ON w.owner_id=i.organization_id
        WHERE w.state='active' AND (($1::text IS NULL AND recipient_id=$2 AND i.state='pending' AND i.expires_at>clock_timestamp()) OR ($1 IS NOT NULL AND i.organization_id=$1))
        AND ($3::text IS NULL OR i.invitation_id>$3) ORDER BY i.invitation_id LIMIT $4")
        .bind(&org).bind(&user).bind(q.after).bind(i64::from(limit)+1).fetch_all(&mut **g.transaction()).await?;
    let visible = &rows[..rows.len().min(usize::from(limit))];
    let data = visible.iter().map(view).collect::<Result<Vec<_>, _>>()?;
    let next = if rows.len() > visible.len() {
        visible
            .last()
            .map(|r| r.try_get::<&str, _>("invitation_id"))
            .transpose()?
    } else {
        None
    };
    g.finish().await?;
    Ok(Json(
        json!({"organization_id":org,"user_id":user,"data":data,"next_cursor":next}),
    ))
}
pub(super) async fn incoming(
    State(i): State<Arc<UserIdentity>>,
    h: HeaderMap,
    q: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    list(&i, &h, None, q.map_err(|_| TeamError::Invalid)?.0).await
}
pub(super) async fn outgoing(
    State(i): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    q: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    list(&i, &h, Some(org), q.map_err(|_| TeamError::Invalid)?.0).await
}
pub(super) async fn accept(
    State(i): State<Arc<UserIdentity>>,
    Path(id): Path<String>,
    h: HeaderMap,
    body: Result<Json<Accept>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    if !body.map_err(|_| TeamError::Invalid)?.0.accept {
        return Err(TeamError::Invalid.into());
    }
    let (mut g, _) = write(&i, &h, Some(&id)).await?;
    let user = g.view().user_id.clone();
    let org:String=sqlx::query_scalar("SELECT organization_id FROM organization_invitations WHERE invitation_id=$1 AND recipient_id=$2").bind(&id).bind(&user).fetch_optional(&mut **g.transaction()).await?.ok_or(TeamError::NotFound)?;
    let o = crate::team::organization_row(g.transaction(), &org, true).await?;
    let version: i64 = o.try_get("policy_version")?;
    let row=sqlx::query("SELECT issuer_id,role,state,expires_at>clock_timestamp() AS live FROM organization_invitations WHERE invitation_id=$1 FOR UPDATE")
        .bind(&id).fetch_one(&mut **g.transaction()).await?;
    let raw: &str = row.try_get("role")?;
    let role: TeamRole = raw.parse().map_err(|_| TeamError::Invalid)?;
    if row.try_get::<&str, _>("state")? == "accepted" {
        if crate::team::membership(g.transaction(), &org, &user).await? != role {
            return Err(TeamError::Forbidden.into());
        }
    } else {
        let issuer: &str = row.try_get("issuer_id")?;
        let (authority, _) = manager(g.transaction(), &org, issuer, true).await?;
        if !rank_can_grant(authority, role) {
            return Err(TeamError::Forbidden.into());
        }
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active)").bind(&org).bind(&user).fetch_one(&mut **g.transaction()).await?;
        if exists
            || row.try_get::<&str, _>("state")? != "pending"
            || !row.try_get::<bool, _>("live")?
        {
            return Err(TeamError::Conflict.into());
        }
        let changed=sqlx::query("UPDATE organization_invitations SET state='accepted',accepted_version=$2 WHERE invitation_id=$1 AND state='pending' AND expires_at>clock_timestamp()").bind(&id).bind(version.checked_add(1).ok_or(TeamError::Invalid)?).execute(&mut **g.transaction()).await?.rows_affected();
        if changed != 1 {
            return Err(TeamError::Conflict.into());
        }
        let result = TeamStore::set_membership_in(
            g.transaction(),
            issuer,
            &org,
            &format!("invitation-{id}"),
            &ChangeMembership {
                user_id: user.clone(),
                role: Some(role),
                expected_policy_version: version,
            },
        )
        .await?;
        if result.policy_version != version + 1 {
            return Err(TeamError::Conflict.into());
        }
        audit(g.transaction(), &org, &user, &id, "invitation.accepted").await?;
    }
    let value = current(g.transaction(), &id).await?;
    g.finish().await?;
    Ok(Json(value))
}
pub(super) async fn decline(
    State(i): State<Arc<UserIdentity>>,
    Path(id): Path<String>,
    h: HeaderMap,
    body: Result<Json<Empty>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(Empty {}) = body.map_err(|_| TeamError::Invalid)?;
    end_invite(&i, &h, None, &id).await
}
pub(super) async fn revoke(
    State(i): State<Arc<UserIdentity>>,
    Path((org, id)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    end_invite(&i, &h, Some(org), &id).await
}
async fn end_invite(
    i: &UserIdentity,
    h: &HeaderMap,
    scope: Option<String>,
    id: &str,
) -> Result<Json<Value>, UserApiError> {
    let (mut g, _) = write(i, h, Some(id)).await?;
    let actor = g.view().user_id.clone();
    let org = if let Some(org) = &scope {
        manager(g.transaction(), org, &actor, true).await?;
        org.clone()
    } else {
        sqlx::query_scalar("SELECT organization_id FROM organization_invitations WHERE invitation_id=$1 AND recipient_id=$2").bind(id).bind(&actor).fetch_optional(&mut **g.transaction()).await?.ok_or(TeamError::NotFound)?
    };
    let row=sqlx::query("SELECT role,state FROM organization_invitations WHERE invitation_id=$1 AND organization_id=$2 FOR UPDATE").bind(id).bind(&org).fetch_optional(&mut **g.transaction()).await?.ok_or(TeamError::NotFound)?;
    let state = if scope.is_some() {
        "revoked"
    } else {
        "declined"
    };
    if scope.is_some() {
        let role: TeamRole = row
            .try_get::<&str, _>("role")?
            .parse()
            .map_err(|_| TeamError::Invalid)?;
        if !rank_can_grant(
            crate::team::membership(g.transaction(), &org, &actor).await?,
            role,
        ) {
            return Err(TeamError::Forbidden.into());
        }
    }
    if row.try_get::<&str, _>("state")? == "pending" {
        sqlx::query("UPDATE organization_invitations SET state=$2 WHERE invitation_id=$1")
            .bind(id)
            .bind(state)
            .execute(&mut **g.transaction())
            .await?;
        audit(
            g.transaction(),
            &org,
            &actor,
            id,
            if scope.is_some() {
                "invitation.revoked"
            } else {
                "invitation.declined"
            },
        )
        .await?;
    } else if row.try_get::<&str, _>("state")? != state {
        return Err(TeamError::Conflict.into());
    }
    let v = current(g.transaction(), id).await?;
    g.finish().await?;
    Ok(Json(v))
}
