//! Team budgets allocate the same organization's spending capacity. No balances are copied into grants.
use super::*;
use crate::auth::users::UserSessionGuard;
use orange_protocol::{Money, team::TeamRole};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    expected_version: String,
    owner_ceiling: Option<Money>,
    hard_limit: Money,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Allocation {
    budget_id: String,
    member_id: String,
    limit: Money,
    expected_policy_version: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Adjustment {
    limit: Money,
    allow_submit: bool,
    expected_version: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    after: Option<String>,
    limit: Option<u16>,
}
fn number(s: &str) -> Result<i64, UserApiError> {
    s.parse::<i64>()
        .ok()
        .filter(|x| *x >= 0 && x.to_string() == s)
        .ok_or_else(|| TeamError::Invalid.into())
}
fn manager(role: TeamRole) -> bool {
    matches!(role, TeamRole::Owner | TeamRole::Admin)
}
async fn role(g: &mut UserSessionGuard, org: &str, write: bool) -> Result<TeamRole, UserApiError> {
    let user = g.view().user_id.clone();
    crate::team::organization_row(g.transaction(), org, write).await?;
    let role = crate::team::membership(g.transaction(), org, &user).await?;
    if role == TeamRole::Viewer || write && !manager(role) {
        return Err(TeamError::Forbidden.into());
    }
    Ok(role)
}
async fn policy_row(
    g: &mut UserSessionGuard,
    org: &str,
) -> Result<sqlx::postgres::PgRow, UserApiError> {
    Ok(sqlx::query("SELECT owner_ceiling,hard_limit,allocated::text AS allocated,version FROM organization_budget_policies WHERE organization_id=$1 FOR SHARE")
        .bind(org).fetch_one(&mut **g.transaction()).await?)
}
fn policy_view(org: &str, r: &sqlx::postgres::PgRow) -> Result<Value, UserApiError> {
    Ok(
        json!({"organization_id":org,"owner_ceiling":r.try_get::<Option<i64>,_>("owner_ceiling")?.map(|v|v.to_string()),"hard_limit":r.try_get::<Option<i64>,_>("hard_limit")?.map(|v|v.to_string()),
        "allocated":r.try_get::<&str,_>("allocated")?,"version":r.try_get::<i64,_>("version")?.to_string()}),
    )
}
pub(super) async fn policy(
    State(i): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let mut g = i.authenticate(&h).await?;
    if role(&mut g, &org, false).await? == TeamRole::Member {
        return Err(TeamError::Forbidden.into());
    }
    let v = policy_view(&org, &policy_row(&mut g, &org).await?)?;
    g.finish().await?;
    Ok(Json(v))
}
pub(super) async fn set_policy(
    State(i): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    body: Result<Json<Policy>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(p) = body.map_err(|_| TeamError::Invalid)?;
    let (mut g, key) = super::team_management::write(&i, &h, None).await?;
    let user = g.view().user_id.clone();
    let current = role(&mut g, &org, true).await?;
    if p.owner_ceiling.is_some() && current != TeamRole::Owner {
        return Err(TeamError::Forbidden.into());
    }
    let digest = orange_protocol::request_digest("team.budget-policy", &p)
        .map_err(|_| TeamError::Invalid)?;
    let viewed = policy_row(&mut g, &org).await?;
    if crate::team::prior::<Value>(
        g.transaction(),
        &user,
        &org,
        "team.budget-policy",
        &key,
        &digest,
    )
    .await?
    .is_none()
    {
        if number(&p.expected_version)? != viewed.try_get::<i64, _>("version")? {
            return Err(TeamError::Version.into());
        }
        let ceiling = p
            .owner_ceiling
            .map(|c| c.0.units())
            .or(viewed.try_get("owner_ceiling")?)
            .ok_or(TeamError::Invalid)?;
        let hard = p.hard_limit.0.units();
        if hard > ceiling {
            return Err(UserApiError(StatusCode::CONFLICT, "TEAM_BUDGET_EXCEEDED"));
        }
        let changed=sqlx::query("UPDATE organization_budget_policies SET hard_limit=$2,owner_ceiling=$3,version=version+1 WHERE organization_id=$1 AND allocated<=$2::bigint")
            .bind(&org).bind(hard).bind(ceiling).execute(&mut **g.transaction()).await?.rows_affected();
        if changed != 1 {
            return Err(UserApiError(StatusCode::CONFLICT, "TEAM_BUDGET_EXCEEDED"));
        }
        crate::team::record(
            g.transaction(),
            &user,
            &org,
            "team.budget-policy",
            &key,
            &digest,
            &json!({"accepted":true}),
        )
        .await?;
        crate::team::audit(
            g.transaction(),
            &org,
            &user,
            "budget.policy_changed",
            &org,
            &digest,
        )
        .await?;
    }
    let v = policy_view(&org, &policy_row(&mut g, &org).await?)?;
    g.finish().await?;
    Ok(Json(v))
}
fn view(row: &sqlx::postgres::PgRow) -> Result<Value, UserApiError> {
    Ok(
        json!({"root_budget_id":row.try_get::<&str,_>("root_budget_id")?,"organization_id":row.try_get::<&str,_>("owner_id")?,"member_id":row.try_get::<Option<&str>,_>("member_id")?,
        "limit":row.try_get::<i64,_>("limit_amount")?.to_string(),"spent":row.try_get::<i64,_>("spent")?.to_string(),"reserved":row.try_get::<i64,_>("reserved")?.to_string(),
        "state":row.try_get::<&str,_>("state")?,"allow_submit":row.try_get::<bool,_>("allow_submit")?,"version":row.try_get::<Option<i64>,_>("version")?.map(|v|v.to_string())}),
    )
}
async fn rows(
    g: &mut UserSessionGuard,
    org: &str,
    role: TeamRole,
    id: Option<&str>,
    after: &str,
    limit: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, UserApiError> {
    let user = g.view().user_id.clone();
    Ok(sqlx::query("SELECT b.root_budget_id,b.owner_id,b.limit_amount,b.spent,b.reserved,b.state,a.member_id,a.version,COALESCE(gr.can_submit,false) AS allow_submit
       FROM budget_roots b LEFT JOIN member_budget_allocations a USING(root_budget_id) LEFT JOIN budget_user_grants gr ON gr.root_budget_id=b.root_budget_id AND gr.user_id=COALESCE(a.member_id,$3)
       WHERE b.owner_id=$1 AND ($2 OR (COALESCE(a.member_id,$3)=$3 AND gr.can_submit)) AND ($4::text IS NULL OR b.root_budget_id=$4) AND b.root_budget_id>$5 ORDER BY b.root_budget_id LIMIT $6")
       .bind(org).bind(role!=TeamRole::Member).bind(&user).bind(id).bind(after).bind(limit).fetch_all(&mut **g.transaction()).await?)
}
async fn one(
    g: &mut UserSessionGuard,
    org: &str,
    actor_role: TeamRole,
    id: &str,
) -> Result<Value, UserApiError> {
    let data = rows(g, org, actor_role, Some(id), "", 1).await?;
    view(data.first().ok_or(TeamError::NotFound)?)
}
pub(super) async fn list(
    State(i): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    q: Result<Query<Page>, QueryRejection>,
) -> Result<Json<Value>, UserApiError> {
    let q = q.map_err(|_| TeamError::Invalid)?.0;
    let n = q.limit.unwrap_or(25);
    if !(1..=100).contains(&n) || q.after.as_ref().is_some_and(|a| !valid_id(a)) {
        return Err(TeamError::Invalid.into());
    }
    let mut g = i.authenticate(&h).await?;
    let r = role(&mut g, &org, false).await?;
    let all = rows(
        &mut g,
        &org,
        r,
        None,
        q.after.as_deref().unwrap_or(""),
        i64::from(n) + 1,
    )
    .await?;
    let visible = &all[..all.len().min(usize::from(n))];
    let cursor = if all.len() > visible.len() {
        visible
            .last()
            .map(|v| v.try_get::<&str, _>("root_budget_id"))
            .transpose()?
    } else {
        None
    };
    let data = visible.iter().map(view).collect::<Result<Vec<_>, _>>()?;
    g.finish().await?;
    Ok(Json(
        json!({"organization_id":org,"data":data,"next_cursor":cursor}),
    ))
}
pub(super) async fn read(
    State(i): State<Arc<UserIdentity>>,
    Path((org, id)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let mut g = i.authenticate(&h).await?;
    let r = role(&mut g, &org, false).await?;
    let v = one(&mut g, &org, r, &id).await?;
    g.finish().await?;
    Ok(Json(v))
}
pub(super) async fn allocate(
    State(i): State<Arc<UserIdentity>>,
    Path(org): Path<String>,
    h: HeaderMap,
    body: Result<Json<Allocation>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(p) = body.map_err(|_| TeamError::Invalid)?;
    let (mut g, _) = super::team_management::write(&i, &h, Some(&p.budget_id)).await?;
    if !uuid::Uuid::parse_str(&p.budget_id)
        .is_ok_and(|v| v.get_version_num() == 4 && v.to_string() == p.budget_id)
        || !valid_id(&p.member_id)
    {
        return Err(TeamError::Invalid.into());
    }
    let user = g.view().user_id.clone();
    let authority = role(&mut g, &org, true).await?;
    let digest = orange_protocol::request_digest("team.member-budget.v1", &(&org, &user, &p))
        .map_err(|_| TeamError::Invalid)?;
    let prior:Option<String>=sqlx::query_scalar("SELECT request_digest FROM member_budget_allocations WHERE root_budget_id=$1 AND organization_id=$2 AND creator_id=$3").bind(&p.budget_id).bind(&org).bind(&user).fetch_optional(&mut **g.transaction()).await?;
    if let Some(prior) = prior {
        if prior != digest {
            return Err(TeamError::Conflict.into());
        }
    } else {
        if !crate::team::membership(g.transaction(), &org, &p.member_id)
            .await?
            .can_create_content()
        {
            return Err(TeamError::Forbidden.into());
        }
        let policy = policy_row(&mut g, &org).await?;
        if policy.try_get::<Option<i64>, _>("hard_limit")?.is_none() {
            return Err(UserApiError(
                StatusCode::CONFLICT,
                "TEAM_BUDGET_NOT_CONFIGURED",
            ));
        }
        if policy.try_get::<i64, _>("version")? != number(&p.expected_policy_version)? {
            return Err(TeamError::Version.into());
        }
        let inserted=sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(&p.budget_id).bind(&org).bind(format!("member-{}",p.budget_id)).bind(p.limit.0.units()).execute(&mut **g.transaction()).await?.rows_affected();
        if inserted != 1 {
            return Err(TeamError::Conflict.into());
        }
        sqlx::query("INSERT INTO member_budget_allocations(root_budget_id,organization_id,member_id,creator_id,request_digest) VALUES($1,$2,$3,$4,$5)")
          .bind(&p.budget_id).bind(&org).bind(&p.member_id).bind(&user).bind(&digest).execute(&mut **g.transaction()).await?;
        sqlx::query(
            "INSERT INTO budget_user_grants(root_budget_id,user_id,can_submit) VALUES($1,$2,true)",
        )
        .bind(&p.budget_id)
        .bind(&p.member_id)
        .execute(&mut **g.transaction())
        .await?;
        crate::team::audit(
            g.transaction(),
            &org,
            &user,
            "budget.allocated",
            &p.budget_id,
            &digest,
        )
        .await?;
    }
    let v = one(&mut g, &org, authority, &p.budget_id).await?;
    g.finish().await?;
    Ok(Json(v))
}
pub(super) async fn adjust(
    State(i): State<Arc<UserIdentity>>,
    Path((org, id)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Adjustment>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let Json(p) = body.map_err(|_| TeamError::Invalid)?;
    let (mut g, key) = super::team_management::write(&i, &h, None).await?;
    let actor = g.view().user_id.clone();
    let authority = role(&mut g, &org, true).await?;
    let digest = orange_protocol::request_digest("team.member-budget.adjust.v1", &(&id, &p))
        .map_err(|_| TeamError::Invalid)?;
    if crate::team::prior::<Value>(
        g.transaction(),
        &actor,
        &org,
        "team.budget-adjust",
        &key,
        &digest,
    )
    .await?
    .is_none()
    {
        let r=sqlx::query("SELECT a.member_id,a.version FROM member_budget_allocations a WHERE a.root_budget_id=$1 AND a.organization_id=$2 FOR UPDATE").bind(&id).bind(&org).fetch_optional(&mut **g.transaction()).await?.ok_or(TeamError::NotFound)?;
        let member: &str = r.try_get("member_id")?;
        if r.try_get::<i64, _>("version")? != number(&p.expected_version)? {
            return Err(TeamError::Version.into());
        }
        if p.allow_submit
            && !crate::team::membership(g.transaction(), &org, member)
                .await?
                .can_create_content()
        {
            return Err(TeamError::Forbidden.into());
        }
        let changed=sqlx::query("UPDATE budget_roots SET limit_amount=$2 WHERE root_budget_id=$1 AND owner_id=$3 AND state='active' AND spent::numeric+reserved<=$2::bigint")
            .bind(&id).bind(p.limit.0.units()).bind(&org).execute(&mut **g.transaction()).await?.rows_affected();
        if changed != 1 {
            return Err(UserApiError(
                StatusCode::CONFLICT,
                "BUDGET_ALREADY_COMMITTED",
            ));
        }
        sqlx::query(
            "UPDATE budget_user_grants SET can_submit=$3 WHERE root_budget_id=$1 AND user_id=$2",
        )
        .bind(&id)
        .bind(member)
        .bind(p.allow_submit)
        .execute(&mut **g.transaction())
        .await?;
        sqlx::query(
            "UPDATE member_budget_allocations SET version=version+1 WHERE root_budget_id=$1 AND version=$2",
        )
        .bind(&id)
        .bind(number(&p.expected_version)?).execute(&mut **g.transaction())
        .await?;
        crate::team::record(
            g.transaction(),
            &actor,
            &org,
            "team.budget-adjust",
            &key,
            &digest,
            &json!({"accepted":true}),
        )
        .await?;
        crate::team::audit(
            g.transaction(),
            &org,
            &actor,
            "budget.adjusted",
            &id,
            &digest,
        )
        .await?;
    }
    let v = one(&mut g, &org, authority, &id).await?;
    g.finish().await?;
    Ok(Json(v))
}
