//! Transactional administrative intent and approval. Device completion is a separate
//! trust boundary: enqueueing a drain never claims that processes or GPUs are free.
use super::{identity::AdminIdentity, *};
use axum::{
    extract::{Path, State, rejection::JsonRejection},
    http::HeaderValue,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;

const POLICY: &str = "admin-node-policy-v1";
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeQuery {
    environment: String,
}
pub async fn get_node(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(node): Path<String>,
    query: Result<axum::extract::Query<NodeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, AdminError> {
    let environment = query.map_err(|_| AdminError::Invalid)?.0.environment;
    if !id(&node) || environment != identity.config.environment {
        return Err(AdminError::Invalid);
    }
    let mut tx = begin(&identity).await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    let row = sqlx::query("SELECT * FROM nodes WHERE node_id=$1 AND environment=$2 FOR SHARE")
        .bind(&node)
        .bind(&environment)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AdminError::NotFound)?;
    let owner: String = row.try_get("owner_id")?;
    let region: String = row.try_get("region_id")?;
    let scope = session
        .grants
        .iter()
        .find(|(scope, permissions)| {
            scope.environment == environment
                && permissions.contains("nodes.read")
                && match scope.kind.as_str() {
                    "platform" => scope.scope_id == "platform",
                    "tenant" => scope.scope_id == owner,
                    "region" => scope.scope_id == region,
                    "resource" => scope.scope_id == node,
                    _ => false,
                }
        })
        .map(|(scope, _)| scope)
        .ok_or(AdminError::Forbidden)?;
    let deadline = actor(&mut tx, &session.view.principal_id, scope, &["nodes.read"]).await?;
    let version = format!("v{}", row.try_get::<i64, _>("version")?);
    let mut doc = json!({"node_id":node,"region_id":region,"status":row.try_get::<String,_>("state")?,"version":version});
    if let Some(health) = row.try_get::<Option<String>, _>("health_report_id")? {
        doc["health_report_id"] = json!(health);
    }
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at) VALUES($1,$2,'node.read',$3,$4,$5,$6,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&session.view.principal_id).bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).bind(&node).execute(&mut *tx).await?;
    identity.current_in(&headers, &mut tx).await?;
    if now(&mut tx).await? >= deadline {
        return Err(AdminError::Forbidden);
    }
    tx.commit().await?;
    response(doc, StatusCode::OK)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub scope: Scope,
    pub target_version: String,
    pub ticket_id: String,
    pub reason: String,
    pub action_type: String,
    pub payload: ActionPayload,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ActionPayload {
    Node(NodePayload),
    Case(case_edit::Edit),
}
mod case_edit;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePayload {
    pub node_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub health_report_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub reason_code: Option<String>,
}
fn non_null<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    decision: String,
    payload_digest: String,
    policy_version: String,
    reason: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Execute {
    payload_digest: String,
    policy_version: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    reason: String,
}

fn id(value: &str) -> bool {
    orange_protocol::valid_id(value)
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
}
fn reason(value: &str, min: usize) -> bool {
    (min..=1000).contains(&value.chars().count()) && !value.trim().is_empty()
}
impl ActionRequest {
    fn policy(&self) -> &'static str {
        match self.payload {
            ActionPayload::Case(_) => "admin-case-policy-v1",
            ActionPayload::Node(_) => POLICY,
        }
    }
    fn read_permission(&self) -> &'static str {
        match self.payload {
            ActionPayload::Case(_) => "cases.read",
            ActionPayload::Node(_) => "nodes.read",
        }
    }
    fn checker_permission(&self) -> Option<&'static str> {
        match (&self.action_type[..], &self.payload) {
            ("node.resume", ActionPayload::Node(_)) => Some("nodes.resume.approve"),
            ("case.update", ActionPayload::Case(case_edit::Edit::Close { .. })) => {
                Some("cases.close.approve")
            }
            _ => None,
        }
    }

    fn permission(&self) -> Result<&'static str, AdminError> {
        match self.action_type.as_str() {
            "case.update" => match &self.payload {
                ActionPayload::Case(edit) => Ok(edit.permission()),
                _ => Err(AdminError::Invalid),
            },
            "node.drain" => Ok("nodes.drain"),
            "node.quarantine" => Ok("nodes.quarantine"),
            "node.resume" => Ok("nodes.resume"),
            _ => Err(AdminError::Invalid),
        }
    }
    fn validate(&self, environment: &str) -> Result<(), AdminError> {
        self.permission()?;
        if self.scope.environment != environment
            || !matches!(
                self.scope.kind.as_str(),
                "platform" | "tenant" | "region" | "resource"
            )
            || !id(&self.scope.scope_id)
            || (self.scope.kind == "platform" && self.scope.scope_id != "platform")
            || !id(&self.ticket_id)
            || !reason(&self.reason, 5)
            || !id(&self.target_version)
        {
            return Err(AdminError::Invalid);
        }
        let node = match &self.payload {
            ActionPayload::Case(edit) if self.action_type == "case.update" => {
                return edit.validate(self);
            }
            ActionPayload::Case(_) => return Err(AdminError::Invalid),
            ActionPayload::Node(node) if id(&node.node_id) => node,
            _ => return Err(AdminError::Invalid),
        };
        let valid_payload = match self.action_type.as_str() {
            "node.drain" => node.health_report_id.is_none() && node.reason_code.is_none(),
            "node.quarantine" => {
                node.health_report_id.is_none()
                    && node.reason_code.as_ref().is_some_and(|v| {
                        (1..=64).contains(&v.chars().count()) && !v.trim().is_empty()
                    })
            }
            "node.resume" => {
                node.reason_code.is_none() && node.health_report_id.as_ref().is_some_and(|v| id(v))
            }
            _ => false,
        };
        if !valid_payload {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}
fn digest<T: Serialize>(operation: &str, body: &T) -> Result<String, AdminError> {
    orange_protocol::request_digest(operation, body).map_err(|_| AdminError::Invalid)
}
fn text<'a>(doc: &'a Value, field: &str) -> Result<&'a str, AdminError> {
    doc[field].as_str().ok_or(AdminError::Unavailable)
}
async fn now(tx: &mut Tx<'_>) -> Result<i64, AdminError> {
    Ok(
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .fetch_one(&mut **tx)
            .await?,
    )
}
async fn begin(identity: &AdminIdentity) -> Result<Tx<'_>, AdminError> {
    let mut tx = identity.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // Low-volume admin mutations have a common lock order, including cross-principal
    // approval checks. No network work is performed while this lock is held.
    sqlx::query("SELECT pg_advisory_xact_lock(684723011)")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
async fn actor(
    tx: &mut Tx<'_>,
    principal: &str,
    scope: &Scope,
    permissions: &[&str],
) -> Result<i64, AdminError> {
    let active: Option<bool> =
        sqlx::query_scalar("SELECT active FROM admin_principals WHERE principal_id=$1 FOR SHARE")
            .bind(principal)
            .fetch_optional(&mut **tx)
            .await?;
    if active != Some(true) {
        return Err(AdminError::Forbidden);
    }
    let rows = sqlx::query("SELECT role,expires_at FROM admin_grants WHERE principal_id=$1 AND environment=$2 AND scope_kind=$3 AND scope_id=$4 AND NOT revoked FOR SHARE")
        .bind(principal).bind(&scope.environment).bind(&scope.kind).bind(&scope.scope_id).fetch_all(&mut **tx).await?;
    let registry = role_permissions();
    let time = now(tx).await?;
    let mut deadline = i64::MAX;
    for permission in permissions {
        let mut until = 0;
        for row in &rows {
            let role: String = row.try_get("role")?;
            if registry.get(&role).is_some_and(|p| p.contains(*permission)) {
                until = until.max(row.try_get::<i64, _>("expires_at")?);
            }
        }
        if until <= time {
            return Err(AdminError::Forbidden);
        }
        deadline = deadline.min(until);
    }
    Ok(deadline)
}
async fn authenticate(
    identity: &AdminIdentity,
    headers: &HeaderMap,
    tx: &mut Tx<'_>,
) -> Result<CurrentSession, AdminError> {
    let session = identity.current_in(headers, tx).await?;
    identity.check_write(headers, &session)?;
    let time = now(tx).await?;
    if session.auth_time > time + 30 || session.auth_time <= time - 300 {
        return Err(AdminError::StepUp);
    }
    Ok(session)
}
async fn finish(
    identity: &AdminIdentity,
    headers: &HeaderMap,
    tx: &mut Tx<'_>,
    deadline: i64,
) -> Result<(), AdminError> {
    // Re-evaluate wall-clock expiry after all locks and writes; authorization rows
    // cannot be changed concurrently while this transaction holds their locks.
    authenticate(identity, headers, tx).await?;
    if now(tx).await? >= deadline {
        return Err(AdminError::Forbidden);
    }
    Ok(())
}
fn key(headers: &HeaderMap) -> Result<&str, AdminError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(AdminError::Invalid)?;
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AdminError::Invalid);
    }
    Ok(key)
}
fn etag(headers: &HeaderMap) -> Result<&str, AdminError> {
    let value = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .ok_or(AdminError::Precondition)?;
    if value.len() > 160 {
        return Err(AdminError::Precondition);
    }
    Ok(value)
}
async fn prior(
    tx: &mut Tx<'_>,
    principal: &str,
    key: &str,
    hash: &str,
) -> Result<Option<String>, AdminError> {
    let row = sqlx::query("SELECT request_digest,action_id FROM admin_action_keys WHERE principal_id=$1 AND request_key=$2")
        .bind(principal).bind(key).fetch_optional(&mut **tx).await?;
    match row {
        None => Ok(None),
        Some(row) if row.try_get::<String, _>("request_digest")? == hash => {
            Ok(Some(row.try_get("action_id")?))
        }
        Some(_) => Err(AdminError::Idempotency),
    }
}
async fn remember(
    tx: &mut Tx<'_>,
    principal: &str,
    key: &str,
    hash: &str,
    action: &str,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_action_keys(principal_id,request_key,request_digest,action_id) VALUES($1,$2,$3,$4)")
        .bind(principal).bind(key).bind(hash).bind(action).execute(&mut **tx).await?;
    Ok(())
}
struct Managed {
    doc: Value,
    request: ActionRequest,
    expires: i64,
    revision: i64,
}
async fn load(tx: &mut Tx<'_>, action: &str) -> Result<Managed, AdminError> {
    if !id(action) {
        return Err(AdminError::Invalid);
    }
    let row = sqlx::query("SELECT a.document,c.expires_at,c.revision FROM admin_actions a JOIN admin_action_control c USING(action_id) WHERE a.action_id=$1 FOR UPDATE OF a,c")
        .bind(action).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    let doc: Value = row.try_get("document")?;
    let request: ActionRequest =
        serde_json::from_value(doc["request"].clone()).map_err(|_| AdminError::Unavailable)?;
    if text(&doc, "policy_version")? != request.policy()
        || digest("admin-action-payload-v1", &request)? != text(&doc, "payload_digest")?
    {
        return Err(AdminError::Conflict);
    }
    Ok(Managed {
        doc,
        request,
        expires: row.try_get("expires_at")?,
        revision: row.try_get("revision")?,
    })
}
async fn event(
    tx: &mut Tx<'_>,
    principal: &str,
    doc: &Value,
    kind: &str,
    why: &str,
) -> Result<(), AdminError> {
    let time = now(tx).await?;
    sqlx::query("INSERT INTO admin_action_events(event_id,action_id,principal_id,kind,reason,document,occurred_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(text(doc,"action_id")?).bind(principal).bind(kind).bind(why).bind(doc).bind(time).execute(&mut **tx).await?;
    let scope = &doc["request"]["scope"];
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(principal).bind(kind).bind(text(scope,"environment")?).bind(text(scope,"kind")?).bind(text(scope,"scope_id")?).bind(text(doc,"action_id")?).bind(time).execute(&mut **tx).await?;
    Ok(())
}
async fn save(tx: &mut Tx<'_>, m: &mut Managed) -> Result<(), AdminError> {
    m.revision = m.revision.checked_add(1).ok_or(AdminError::Conflict)?;
    m.doc["version"] = json!(format!("v{}", m.revision));
    sqlx::query("UPDATE admin_actions SET document=$2 WHERE action_id=$1")
        .bind(text(&m.doc, "action_id")?)
        .bind(&m.doc)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE admin_action_control SET revision=$2 WHERE action_id=$1")
        .bind(text(&m.doc, "action_id")?)
        .bind(m.revision)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
fn response(doc: Value, status: StatusCode) -> Result<Response, AdminError> {
    let tag = HeaderValue::from_str(&format!("\"{}\"", text(&doc, "version")?))
        .map_err(|_| AdminError::Unavailable)?;
    let mut response = (status, Json(doc)).into_response();
    response.headers_mut().insert("etag", tag);
    Ok(response)
}
async fn target(tx: &mut Tx<'_>, request: &ActionRequest) -> Result<(i64, String), AdminError> {
    let node = match &request.payload {
        ActionPayload::Case(edit) => return case_edit::target(tx, request, edit).await,
        ActionPayload::Node(node) => node,
    };
    let row = sqlx::query("SELECT owner_id,environment,region_id,version,state FROM nodes WHERE node_id=$1 FOR UPDATE")
        .bind(&node.node_id).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    let belongs = row.try_get::<String, _>("environment")? == request.scope.environment
        && match request.scope.kind.as_str() {
            "platform" => request.scope.scope_id == "platform",
            "tenant" => row.try_get::<String, _>("owner_id")? == request.scope.scope_id,
            "region" => row.try_get::<String, _>("region_id")? == request.scope.scope_id,
            "resource" => node.node_id == request.scope.scope_id,
            _ => false,
        };
    if !belongs {
        return Err(AdminError::NotFound);
    }
    let version: i64 = row.try_get("version")?;
    if request.target_version != format!("v{version}") {
        return Err(AdminError::Precondition);
    }
    Ok((version, row.try_get("state")?))
}
pub async fn create(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    body: Result<Json<ActionRequest>, JsonRejection>,
) -> Result<Response, AdminError> {
    let request = body.map_err(|_| AdminError::Invalid)?.0;
    request.validate(&identity.config.environment)?;
    let key = key(&headers)?;
    let hash = digest("admin-create-v1", &request)?;
    let mut tx = begin(&identity).await?;
    let session = authenticate(&identity, &headers, &mut tx).await?;
    let principal = &session.view.principal_id;
    let deadline = actor(
        &mut tx,
        principal,
        &request.scope,
        &["actions.create", request.permission()?],
    )
    .await?;
    let doc = if let Some(action) = prior(&mut tx, principal, key, &hash).await? {
        load(&mut tx, &action).await?.doc
    } else {
        target(&mut tx, &request).await?;
        let time = now(&mut tx).await?;
        let action = uuid::Uuid::new_v4().to_string();
        let dual = request.checker_permission().is_some();
        let doc = json!({"action_id":action,"requested_by":principal,"request":request,
            "payload_digest":digest("admin-action-payload-v1",&request)?,"policy_version":request.policy(),
            "approval_policy":if dual {"maker_checker"} else {"single_operator"},"required_approvals":if dual {1} else {0},
            "approvals":[],"state":if dual {"pending_approval"} else {"ready"},"version":"v1",
            "created_at":timestamp(time)?,"expires_at":timestamp(time+900)?});
        sqlx::query("INSERT INTO admin_actions(action_id,environment,scope_kind,scope_id,action_type,document) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(&action).bind(&request.scope.environment).bind(&request.scope.kind).bind(&request.scope.scope_id).bind(&request.action_type).bind(&doc).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO admin_action_control(action_id,expires_at) VALUES($1,$2)")
            .bind(&action)
            .bind(time + 900)
            .execute(&mut *tx)
            .await?;
        remember(&mut tx, principal, key, &hash, &action).await?;
        event(&mut tx, principal, &doc, "action.created", &request.reason).await?;
        doc
    };
    finish(&identity, &headers, &mut tx, deadline).await?;
    tx.commit().await?;
    response(doc, StatusCode::CREATED)
}
pub async fn get_one(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(action): Path<String>,
) -> Result<Response, AdminError> {
    let mut tx = begin(&identity).await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    let m = load(&mut tx, &action).await?;
    let deadline = actor(
        &mut tx,
        &session.view.principal_id,
        &m.request.scope,
        &["actions.read", m.request.read_permission()],
    )
    .await?;
    if now(&mut tx).await? >= deadline {
        return Err(AdminError::Forbidden);
    }
    identity.current_in(&headers, &mut tx).await?;
    event(
        &mut tx,
        &session.view.principal_id,
        &m.doc,
        "action.read",
        "",
    )
    .await?;
    tx.commit().await?;
    response(m.doc, StatusCode::OK)
}

enum Mutation {
    Decide(Decision),
    Execute(Execute),
    Cancel(Cancel),
}
async fn mutate(
    identity: &AdminIdentity,
    headers: &HeaderMap,
    action: &str,
    mutation: Mutation,
) -> Result<Response, AdminError> {
    let key = key(headers)?;
    let expected = etag(headers)?;
    let (kind, body, permission) = match &mutation {
        Mutation::Decide(v) => ("approve", serde_json::to_value(v), "actions.approve"),
        Mutation::Execute(v) => ("execute", serde_json::to_value(v), "actions.execute"),
        Mutation::Cancel(v) => ("cancel", serde_json::to_value(v), "actions.cancel"),
    };
    let hash = digest(
        "admin-mutation-v1",
        &json!({"action":action,"operation":kind,"body":body.map_err(|_|AdminError::Invalid)?,"if_match":expected}),
    )?;
    let mut tx = begin(identity).await?;
    let session = authenticate(identity, headers, &mut tx).await?;
    let principal = &session.view.principal_id;
    let mut m = load(&mut tx, action).await?;
    m.request.validate(&identity.config.environment)?;
    let specific = if matches!(mutation, Mutation::Decide(_)) {
        m.request
            .checker_permission()
            .ok_or(AdminError::Forbidden)?
    } else {
        m.request.permission()?
    };
    let mut deadline = actor(
        &mut tx,
        principal,
        &m.request.scope,
        &[permission, specific],
    )
    .await?;
    if matches!(mutation, Mutation::Decide(_))
        && (m.request.checker_permission().is_none() || text(&m.doc, "requested_by")? == principal)
    {
        return Err(AdminError::Forbidden);
    }
    // Execution always revalidates the maker and every independent approval, even
    // for an idempotent retry after grants have been revoked.
    if matches!(mutation, Mutation::Execute(_) | Mutation::Decide(_)) {
        deadline = deadline.min(
            actor(
                &mut tx,
                text(&m.doc, "requested_by")?,
                &m.request.scope,
                &["actions.create", m.request.permission()?],
            )
            .await?,
        );
        if matches!(mutation, Mutation::Execute(_))
            && let Some(checker) = m.request.checker_permission()
        {
            let approvals = m.doc["approvals"]
                .as_array()
                .ok_or(AdminError::Unavailable)?;
            if approvals.len() != 1
                || approvals[0]["decision"] != "approve"
                || approvals[0]["approver_id"] == m.doc["requested_by"]
                || approvals[0]["payload_digest"] != m.doc["payload_digest"]
                || approvals[0]["policy_version"] != m.doc["policy_version"]
                || approvals[0]["expires_at"] != m.doc["expires_at"]
            {
                return Err(AdminError::Conflict);
            }
            deadline = deadline.min(
                actor(
                    &mut tx,
                    text(&approvals[0], "approver_id")?,
                    &m.request.scope,
                    &["actions.approve", checker],
                )
                .await?,
            );
        }
    }
    let replay = prior(&mut tx, principal, key, &hash).await?;
    if replay.as_ref().is_some_and(|a| a != action) {
        return Err(AdminError::Conflict);
    }
    if replay.is_none() {
        if expected != format!("\"{}\"", text(&m.doc, "version")?) {
            return Err(AdminError::Precondition);
        }
        if now(&mut tx).await? >= m.expires {
            return Err(AdminError::Conflict);
        }
        let state = text(&m.doc, "state")?.to_owned();
        let why;
        match mutation {
            Mutation::Decide(decision) => {
                if state != "pending_approval"
                    || !matches!(decision.decision.as_str(), "approve" | "reject")
                    || !reason(&decision.reason, 1)
                {
                    return Err(AdminError::Conflict);
                }
                check_preview(&m.doc, &decision.payload_digest, &decision.policy_version)?;
                target(&mut tx, &m.request).await?;
                let time = now(&mut tx).await?;
                m.doc["approvals"] = json!([{"approval_id":uuid::Uuid::new_v4().to_string(),"approver_id":principal,"decision":decision.decision,"payload_digest":decision.payload_digest,"policy_version":decision.policy_version,"decided_at":timestamp(time)?,"expires_at":timestamp(m.expires)?}]);
                m.doc["state"] = json!(if decision.decision == "approve" {
                    "ready"
                } else {
                    "rejected"
                });
                why = decision.reason;
            }
            Mutation::Cancel(cancel) => {
                if !matches!(state.as_str(), "pending_approval" | "ready")
                    || !reason(&cancel.reason, 1)
                {
                    return Err(AdminError::Conflict);
                }
                m.doc["state"] = json!("cancelled");
                why = cancel.reason;
            }
            Mutation::Execute(execute) => {
                check_preview(&m.doc, &execute.payload_digest, &execute.policy_version)?;
                if state != "ready" {
                    return Err(AdminError::Conflict);
                }
                let (version, node_state) = target(&mut tx, &m.request).await?;
                match &m.request.payload {
                    ActionPayload::Case(edit) => {
                        deadline = deadline.min(
                            case_edit::apply(
                                &mut tx,
                                edit,
                                version,
                                &node_state,
                                principal,
                                action,
                                &m.request.scope,
                            )
                            .await?,
                        );
                        m.doc["business_ref"] = json!(edit.case_id());
                        m.doc["state"] = json!("succeeded");
                        why = "Reconciliation case metadata updated".into();
                    }
                    ActionPayload::Node(node) => {
                        // A health_report_id string is not verified device recovery evidence.
                        // Resume remains unavailable until the trusted probe consumer exists.
                        if m.request.action_type == "node.resume" {
                            return Err(AdminError::Unavailable);
                        }
                        if node_state == "revoked"
                            || (m.request.action_type == "node.drain"
                                && !matches!(node_state.as_str(), "online" | "offline"))
                        {
                            return Err(AdminError::Conflict);
                        }
                        let command = uuid::Uuid::new_v4().to_string();
                        let new_state = if m.request.action_type == "node.drain" {
                            "draining"
                        } else {
                            "quarantined"
                        };
                        let generation: i64 = sqlx::query_scalar("UPDATE nodes SET state=$2,version=version+1,policy_generation=policy_generation+1 WHERE node_id=$1 RETURNING policy_generation")
                    .bind(&node.node_id)
                    .bind(new_state)
                    .fetch_one(&mut *tx)
                    .await?;
                        sqlx::query("INSERT INTO admin_node_commands(command_id,action_id,node_id,command_kind,target_version,state,created_at,policy_generation) VALUES($1,$2,$3,$4,$5,'queued',$6,$7)")
                    .bind(&command).bind(action).bind(&node.node_id).bind(&m.request.action_type).bind(version).bind(now(&mut tx).await?).bind(generation).execute(&mut *tx).await?;
                        m.doc["state"] = json!("executing");
                        m.doc["business_ref"] = json!(command);
                        why = "Durable node command queued; device completion remains unconfirmed"
                            .into();
                    }
                }
            }
        }
        save(&mut tx, &mut m).await?;
        remember(&mut tx, principal, key, &hash, action).await?;
        event(&mut tx, principal, &m.doc, &format!("action.{kind}"), &why).await?;
    }
    finish(identity, headers, &mut tx, deadline).await?;
    tx.commit().await?;
    response(
        m.doc,
        if kind == "execute" {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
    )
}
fn check_preview(doc: &Value, hash: &str, policy: &str) -> Result<(), AdminError> {
    if hash != text(doc, "payload_digest")? || policy != text(doc, "policy_version")? {
        return Err(AdminError::Conflict);
    }
    Ok(())
}
pub async fn decide(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(action): Path<String>,
    body: Result<Json<Decision>, JsonRejection>,
) -> Result<Response, AdminError> {
    mutate(
        &identity,
        &headers,
        &action,
        Mutation::Decide(body.map_err(|_| AdminError::Invalid)?.0),
    )
    .await
}
pub async fn execute(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(action): Path<String>,
    body: Result<Json<Execute>, JsonRejection>,
) -> Result<Response, AdminError> {
    mutate(
        &identity,
        &headers,
        &action,
        Mutation::Execute(body.map_err(|_| AdminError::Invalid)?.0),
    )
    .await
}
pub async fn cancel(
    State(identity): State<Arc<AdminIdentity>>,
    headers: HeaderMap,
    Path(action): Path<String>,
    body: Result<Json<Cancel>, JsonRejection>,
) -> Result<Response, AdminError> {
    mutate(
        &identity,
        &headers,
        &action,
        Mutation::Cancel(body.map_err(|_| AdminError::Invalid)?.0),
    )
    .await
}
