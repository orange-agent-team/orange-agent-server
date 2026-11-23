//! A support operator may request a single independently approved grant against a
//! user's exact immutable consent. Grant metadata and artifact reads are separate.
use super::*;
use sqlx::postgres::PgRow;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    subject_user_id: String,
    user_authorization_id: String,
    resource_ids: Vec<String>,
    content_fields: Vec<String>,
    pub(super) expires_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revoke {
    pub(super) grant_id: String,
}
pub(super) fn seconds(at: &str) -> Result<i64, AdminError> {
    let time = OffsetDateTime::parse(at, &Rfc3339).map_err(|_| AdminError::Invalid)?;
    if !at.ends_with('Z') || time.nanosecond() != 0 {
        return Err(AdminError::Invalid);
    }
    Ok(time.unix_timestamp())
}
fn scope_ok(scope: &Scope) -> bool {
    scope.environment == "dev" && scope.kind == "tenant" && id(&scope.scope_id)
}
impl Issue {
    pub(super) fn validate(&self, r: &ActionRequest) -> Result<(), AdminError> {
        if !scope_ok(&r.scope)
            || !id(&self.subject_user_id)
            || !id(&self.user_authorization_id)
            || !orange_protocol::valid_digest(&r.target_version)
            || self.content_fields != ["file_content"]
            || !(1..=20).contains(&self.resource_ids.len())
            || self.resource_ids.iter().any(|s| !id(s))
            || self
                .resource_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.resource_ids.len()
        {
            return Err(AdminError::Invalid);
        }
        seconds(&self.expires_at)?;
        Ok(())
    }
}
impl Revoke {
    pub(super) fn validate(&self, r: &ActionRequest) -> Result<(), AdminError> {
        if !scope_ok(&r.scope) || !id(&self.grant_id) || r.target_version != "v1" {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}
async fn consent(tx: &mut Tx<'_>, auth: &str, scope: &Scope) -> Result<PgRow, AdminError> {
    if !scope_ok(scope) || !id(auth) {
        return Err(AdminError::Invalid);
    }
    let row = sqlx::query(
        "SELECT c.* FROM personal_support_consents c JOIN users u USING(user_id)
        JOIN owners o ON o.owner_id=c.owner_id WHERE c.authorization_id=$1 AND c.owner_id=$2
          AND u.personal_owner_id=c.owner_id AND u.active AND o.state='active' FOR SHARE OF c,u,o",
    )
    .bind(auth)
    .bind(&scope.scope_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AdminError::NotFound)?;
    // Acquire the same consent lock as user revocation before checking its receipt.
    let revoked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM personal_support_revocations WHERE authorization_id=$1)",
    )
    .bind(auth)
    .fetch_one(&mut **tx)
    .await?;
    if revoked || row.try_get::<i64, _>("expires_at")? <= now(tx).await? {
        return Err(AdminError::Conflict);
    }
    let list = row.try_get::<Value, _>("resource_versions")?;
    for item in list.as_array().ok_or(AdminError::Unavailable)? {
        let found=sqlx::query("SELECT artifact_id FROM artifacts WHERE artifact_id=$1 AND version_id=$2 AND owner_id=$3 AND creator_id=$4
            AND state='available' AND orange_artifact_content_live(artifact_id) AND media_type='text/plain' AND resource_id IS NULL FOR SHARE")
            .bind(text(item,"artifact_id")?).bind(text(item,"version_id")?).bind(&scope.scope_id)
            .bind(row.try_get::<String,_>("user_id")?).fetch_optional(&mut **tx).await?;
        if found.is_none() {
            return Err(AdminError::Conflict);
        }
    }
    Ok(row)
}
fn preview(row: &PgRow) -> Result<Value, AdminError> {
    let data = json!({"subject_user_id":row.try_get::<String,_>("user_id")?,
        "owner_id":row.try_get::<String,_>("owner_id")?, "ticket_id":row.try_get::<String,_>("ticket_id")?,
        "user_authorization_id":row.try_get::<String,_>("authorization_id")?, "purpose":row.try_get::<String,_>("purpose")?,
        "resources":row.try_get::<Value,_>("resource_versions")?, "content_fields":["file_content"],
        "expires_at":timestamp(row.try_get("expires_at")?)?});
    let version = digest("admin.support-consent.v1", &data)?;
    let mut result = data;
    result["target_version"] = json!(version);
    Ok(result)
}
pub(super) async fn check_issue(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Issue,
) -> Result<(), AdminError> {
    let row = consent(tx, &p.user_authorization_id, &r.scope).await?;
    let view = preview(&row)?;
    if view["target_version"] != r.target_version {
        return Err(AdminError::Precondition);
    }
    if view["subject_user_id"] != p.subject_user_id || view["ticket_id"] != r.ticket_id {
        return Err(AdminError::Forbidden);
    }
    let mut requested = p.resource_ids.clone();
    requested.sort();
    let mut authorized: Vec<&str> = view["resources"]
        .as_array()
        .ok_or(AdminError::Unavailable)?
        .iter()
        .map(|v| text(v, "artifact_id"))
        .collect::<Result<_, _>>()?;
    authorized.sort();
    if requested != authorized || seconds(&p.expires_at)? > row.try_get::<i64, _>("expires_at")? {
        return Err(AdminError::Forbidden);
    }
    if seconds(&p.expires_at)? <= now(tx).await? {
        return Err(AdminError::Conflict);
    }
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM admin_support_grants WHERE authorization_id=$1)",
    )
    .bind(&p.user_authorization_id)
    .fetch_one(&mut **tx)
    .await?;
    if used {
        return Err(AdminError::Conflict);
    }
    Ok(())
}
pub(super) async fn issue(
    tx: &mut Tx<'_>,
    p: &Issue,
    action: &str,
    maker: &str,
    checker: &str,
) -> Result<String, AdminError> {
    let grant = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO admin_support_grants(grant_id,authorization_id,issue_action_id,beneficiary_id,checker_id,expires_at)
        VALUES($1,$2,$3,$4,$5,$6)").bind(&grant).bind(&p.user_authorization_id).bind(action).bind(maker).bind(checker)
        .bind(seconds(&p.expires_at)?).execute(&mut **tx).await?;
    Ok(grant)
}
async fn locked_grant(tx: &mut Tx<'_>, id: &str, scope: &Scope) -> Result<PgRow, AdminError> {
    sqlx::query("SELECT g.*,c.ticket_id,c.resource_versions,c.user_id,c.owner_id,c.expires_at AS user_expires
        FROM admin_support_grants g JOIN personal_support_consents c USING(authorization_id)
        WHERE g.grant_id=$1 AND c.owner_id=$2 FOR SHARE OF g,c").bind(id).bind(&scope.scope_id)
        .fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)
}
pub(super) async fn check_revoke(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Revoke,
) -> Result<(), AdminError> {
    let row = locked_grant(tx, &p.grant_id, &r.scope).await?;
    if row.try_get::<String, _>("ticket_id")? != r.ticket_id {
        return Err(AdminError::Forbidden);
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM admin_support_revocations WHERE grant_id=$1) OR EXISTS(SELECT 1 FROM admin_support_authority_losses WHERE grant_id=$1)",
    )
    .bind(&p.grant_id)
    .fetch_one(&mut **tx)
    .await?
    {
        return Err(AdminError::Conflict);
    }
    Ok(())
}
pub(super) async fn revoke(tx: &mut Tx<'_>, p: &Revoke, action: &str) -> Result<(), AdminError> {
    sqlx::query("SELECT grant_id FROM admin_support_grants WHERE grant_id=$1 FOR UPDATE")
        .bind(&p.grant_id)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO admin_support_revocations(grant_id,revoke_action_id) VALUES($1,$2)")
        .bind(&p.grant_id)
        .bind(action)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
}
impl ScopeQuery {
    fn selected(self) -> Result<Scope, AdminError> {
        let scope = Scope {
            environment: self.environment,
            kind: self.scope_kind,
            scope_id: self.scope_id,
        };
        if !scope_ok(&scope) {
            return Err(AdminError::Invalid);
        }
        Ok(scope)
    }
}
pub async fn review(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<axum::extract::Query<ScopeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let scope = query.map_err(|_| AdminError::Invalid)?.0.selected()?;
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("support.read", &scope)?;
    let info = preview(&consent(&mut tx, &id, &scope).await?)?;
    audit_read(
        &mut tx,
        &session.view.principal_id,
        &scope,
        &id,
        "support.authorization_reviewed",
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("support.read", &scope)?;
    tx.commit().await?;
    Ok(Json(info))
}
async fn audit_read(
    tx: &mut Tx<'_>,
    who: &str,
    scope: &Scope,
    id: &str,
    kind: &str,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at)
        VALUES($1,$2,$3,$4,$5,$6,$7,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(who).bind(kind).bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id)
        .bind(id).execute(&mut **tx).await?;
    Ok(())
}
async fn grant_view(tx: &mut Tx<'_>, scope: &Scope, id: &str) -> Result<Value, AdminError> {
    let row = locked_grant(tx, id, scope).await?;
    let expires: i64 = row.try_get("expires_at")?;
    let grant_revoked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM admin_support_revocations WHERE grant_id=$1) OR EXISTS(SELECT 1 FROM admin_support_authority_losses WHERE grant_id=$1)",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let user_revoked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM personal_support_revocations WHERE authorization_id=$1)",
    )
    .bind(row.try_get::<String, _>("authorization_id")?)
    .fetch_one(&mut **tx)
    .await?;
    let status = if grant_revoked || user_revoked {
        "revoked"
    } else if expires <= now(tx).await? {
        "expired"
    } else {
        let authorized = async {
            actor(
                tx,
                row.try_get("beneficiary_id")?,
                scope,
                &["support.read", "support.grant", "support.content.read"],
            )
            .await?;
            actor(
                tx,
                row.try_get("checker_id")?,
                scope,
                &["actions.approve", "support.grant.approve"],
            )
            .await?;
            consent(tx, row.try_get("authorization_id")?, scope).await
        }
        .await;
        match authorized {
            Ok(_) => "active",
            Err(AdminError::Conflict | AdminError::NotFound | AdminError::Forbidden) => {
                "unavailable"
            }
            Err(e) => return Err(e),
        }
    };
    let resources: Value = row.try_get("resource_versions")?;
    let ids: Vec<&str> = resources
        .as_array()
        .ok_or(AdminError::Unavailable)?
        .iter()
        .map(|v| text(v, "artifact_id"))
        .collect::<Result<_, _>>()?;
    Ok(
        json!({"grant_id":id,"subject_user_id":row.try_get::<String,_>("user_id")?,"resource_ids":ids,"content_fields":["file_content"],
        "expires_at":timestamp(expires)?,"status":status,"version":if grant_revoked{"v2"}else{"v1"}}),
    )
}
pub async fn detail(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<axum::extract::Query<ScopeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let scope = query.map_err(|_| AdminError::Invalid)?.0.selected()?;
    if !super::id(&id) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("support.read", &scope)?;
    let mut value = grant_view(&mut tx, &scope, &id).await?;
    let reference = locked_grant(&mut tx, &id, &scope).await?;
    for (field, column) in [
        ("owner_id", "owner_id"),
        ("ticket_id", "ticket_id"),
        ("user_authorization_id", "authorization_id"),
        ("beneficiary_id", "beneficiary_id"),
        ("checker_id", "checker_id"),
    ] {
        value[field] = json!(reference.try_get::<String, _>(column)?);
    }
    value["resources"] = reference.try_get::<Value, _>("resource_versions")?;
    audit_read(
        &mut tx,
        &session.view.principal_id,
        &scope,
        &id,
        "support.grant_read",
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("support.read", &scope)?;
    tx.commit().await?;
    Ok(Json(value))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    grant_id: String,
    resource_id: String,
    content_field: String,
    offset: usize,
    limit_bytes: usize,
}

#[derive(PartialEq, Eq)]
struct Object {
    scope: Scope,
    root: String,
    key: String,
    size: u64,
    sha: String,
    version: String,
}
async fn authorize_read(tx: &mut Tx<'_>, r: &Read, who: &str) -> Result<(Object, i64), AdminError> {
    let owner:String=sqlx::query_scalar("SELECT c.owner_id FROM admin_support_grants g JOIN personal_support_consents c USING(authorization_id)
        WHERE g.grant_id=$1 AND g.beneficiary_id=$2").bind(&r.grant_id).bind(who)
        .fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    let scope = Scope {
        scope_id: owner,
        kind: "tenant".into(),
        environment: "dev".into(),
    };
    let row = locked_grant(tx, &r.grant_id, &scope).await?;
    let actor_deadline = actor(
        tx,
        who,
        &scope,
        &["support.read", "support.grant", "support.content.read"],
    )
    .await?;
    let checker: &str = row.try_get("checker_id")?;
    let mut deadline = actor_deadline.min(
        actor(
            tx,
            checker,
            &scope,
            &["actions.approve", "support.grant.approve"],
        )
        .await?,
    );
    deadline = deadline
        .min(row.try_get::<i64, _>("expires_at")?)
        .min(row.try_get::<i64, _>("user_expires")?);
    if deadline <= now(tx).await? {
        return Err(AdminError::Forbidden);
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM admin_support_revocations WHERE grant_id=$1) OR EXISTS(SELECT 1 FROM admin_support_authority_losses WHERE grant_id=$1)",
    )
    .bind(&r.grant_id)
    .fetch_one(&mut **tx)
    .await?
    {
        return Err(AdminError::Forbidden);
    }
    let consent = consent(tx, row.try_get("authorization_id")?, &scope).await?;
    let list: Value = consent.try_get("resource_versions")?;
    let v = list
        .as_array()
        .ok_or(AdminError::Unavailable)?
        .iter()
        .find(|item| item["artifact_id"] == r.resource_id)
        .ok_or(AdminError::Forbidden)?;
    let version = text(v, "version_id")?;
    let object=sqlx::query("SELECT a.root_id,a.size_bytes,a.sha256,t.published_key FROM artifacts a JOIN artifact_transfers t ON a.published_transfer_id=t.transfer_id
        WHERE a.artifact_id=$1 AND a.version_id=$2 AND a.owner_id=$3 AND a.creator_id=$4 AND a.state='available' AND orange_artifact_content_live(a.artifact_id) AND a.resource_id IS NULL
          AND a.media_type='text/plain' AND t.artifact_id=a.artifact_id AND t.state='attached' AND NOT t.published_deleted FOR SHARE OF a,t")
        .bind(&r.resource_id).bind(version).bind(&scope.scope_id).bind(consent.try_get::<String,_>("user_id")?)
        .fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    Ok((
        Object {
            scope,
            root: object.try_get("root_id")?,
            key: object.try_get("published_key")?,
            sha: object.try_get("sha256")?,
            size: u64::try_from(object.try_get::<i64, _>("size_bytes")?)
                .map_err(|_| AdminError::Unavailable)?,
            version: version.to_owned(),
        },
        deadline,
    ))
}

pub async fn read_content(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    body: Result<Json<Read>, JsonRejection>,
) -> Result<Json<Value>, AdminError> {
    let r = body.map_err(|_| AdminError::Invalid)?.0;
    if r.content_field != "file_content"
        || !id(&r.grant_id)
        || !id(&r.resource_id)
        || !(1..=65536).contains(&r.limit_bytes)
    {
        return Err(AdminError::Invalid);
    }
    let key = key(&headers)?;
    let request_digest = digest("admin.support-read.v1", &r)?;
    let mut tx = begin(&identity).await?;
    let who = authenticate(&identity, &headers, &mut tx)
        .await?
        .view
        .principal_id;
    let (object, deadline) = authorize_read(&mut tx, &r, &who).await?;
    finish(&identity, &headers, &mut tx, deadline).await?;
    tx.commit().await?;
    let reader = identity
        .support_reader
        .as_ref()
        .ok_or(AdminError::Unavailable)?;
    let (content, truncated) = reader
        .support_excerpt(
            &object.root,
            &object.key,
            object.size,
            &object.sha,
            r.offset,
            r.limit_bytes,
        )
        .await
        .map_err(|e| match e {
            crate::artifacts::ArtifactError::Invalid => AdminError::Invalid,
            _ => AdminError::Unavailable,
        })?;
    // I/O held no database lock. Fence every source, actor, session and deadline
    // again before the response and its audit receipt can leave the transaction.
    let mut tx = begin(&identity).await?;
    if authenticate(&identity, &headers, &mut tx)
        .await?
        .view
        .principal_id
        != who
    {
        return Err(AdminError::Forbidden);
    }
    let (current, deadline) = authorize_read(&mut tx, &r, &who).await?;
    if current != object {
        return Err(AdminError::Conflict);
    }
    let old:Option<(String,String)>=sqlx::query_as("SELECT request_digest,audit_event_id FROM admin_support_reads WHERE principal_id=$1 AND request_key=$2")
        .bind(&who).bind(key).fetch_optional(&mut *tx).await?;
    let event = if let Some((prior, event)) = old {
        if prior != request_digest {
            return Err(AdminError::Idempotency);
        }
        event
    } else {
        let event = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at)
            VALUES($1,$2,'support.content_read','dev','tenant',$3,$4,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
            .bind(&event).bind(&who).bind(&current.scope.scope_id).bind(&r.grant_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO admin_support_reads(principal_id,request_key,request_digest,audit_event_id,grant_id,artifact_id,version_id,offset_bytes,limit_bytes,returned_bytes)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(&who).bind(key).bind(request_digest).bind(&event).bind(&r.grant_id).bind(&r.resource_id).bind(&current.version)
            .bind(i64::try_from(r.offset).map_err(|_|AdminError::Invalid)?).bind(r.limit_bytes as i32).bind(content.len() as i32)
            .execute(&mut *tx).await?;
        event
    };
    finish(&identity, &headers, &mut tx, deadline).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"grant_id":r.grant_id,"resource_id":r.resource_id,"content_field":"file_content","content":content,"truncated":truncated,"audit_event_id":event}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryQuery {
    environment: String,
    scope_kind: String,
    scope_id: String,
    limit: Option<u16>,
    cursor: Option<String>,
}
pub async fn directory(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<DirectoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let q = query.map_err(|_| AdminError::Invalid)?.0;
    let scope = ScopeQuery {
        environment: q.environment,
        scope_kind: q.scope_kind,
        scope_id: q.scope_id,
    }
    .selected()?;
    let limit = q.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("support.read", &scope)?;
    let fingerprint = digest(
        "admin.support-grants-page.v1",
        &(
            &scope,
            &session.view.principal_id,
            &session.view.policy_version,
            session.grants.get(&scope),
        ),
    )?;
    let session_hash = hash_secret(&cookie(&headers, SESSION_COOKIE)?);
    let (upper, before) = if let Some(cursor) = q.cursor {
        if cursor.is_empty() || cursor.len() > 2048 {
            return Err(AdminError::Invalid);
        }
        let (upper,before):(i64,i64)=sqlx::query_as("SELECT snapshot_sequence,before_sequence FROM admin_audit_cursors WHERE cursor_hash=$1 AND session_hash=$2
            AND authority=$3 AND principal_id=$4 AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .bind(hash_secret(&cursor)).bind(&session_hash).bind(&fingerprint).bind(&session.view.principal_id)
            .fetch_optional(&mut *tx).await?.ok_or(AdminError::Invalid)?;
        (upper, Some(before))
    } else {
        let upper: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(g.sequence),0) FROM admin_support_grants g
            JOIN personal_support_consents c USING(authorization_id) WHERE c.owner_id=$1",
        )
        .bind(&scope.scope_id)
        .fetch_one(&mut *tx)
        .await?;
        (upper, None)
    };
    let entries:Vec<(String,i64)>=sqlx::query_as("SELECT g.grant_id,g.sequence FROM admin_support_grants g JOIN personal_support_consents c USING(authorization_id)
        WHERE c.owner_id=$1 AND g.sequence<=$2 AND ($3::bigint IS NULL OR g.sequence<$3)
        ORDER BY g.sequence DESC LIMIT $4").bind(&scope.scope_id).bind(upper).bind(before).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
    let mut data = Vec::new();
    let visible = &entries[..entries.len().min(usize::from(limit))];
    for (id, _) in visible {
        data.push(grant_view(&mut tx, &scope, id).await?);
    }
    let mut value = json!({"data":data});
    if entries.len() > visible.len() {
        let (_, last) = visible.last().ok_or(AdminError::Unavailable)?;
        value["next_cursor"] = json!(
            crate::admin::routes::audit::next_cursor(
                &mut tx,
                &session_hash,
                &session,
                &fingerprint,
                upper,
                *last
            )
            .await?
        );
    }
    audit_read(
        &mut tx,
        &session.view.principal_id,
        &scope,
        &scope.scope_id,
        "support.grants_listed",
    )
    .await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("support.read", &scope)?;
    tx.commit().await?;
    Ok(Json(value))
}
