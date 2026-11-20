//! A completed Admin action authorizes a test refund dispatch. External settlement has its own receipt.
use super::*;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Amount {
    currency: String,
    minor_units: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Refund {
    payment_order_id: String,
    amount: Amount,
    reconciliation_case_id: String,
}
fn valid_scope(scope: &Scope) -> bool {
    scope.environment == "dev"
        && ((scope.kind == "platform" && scope.scope_id == "platform")
            || (scope.kind == "tenant" && id(&scope.scope_id)))
}
impl Refund {
    pub(super) fn validate(&self, r: &ActionRequest) -> Result<(), AdminError> {
        if !valid_scope(&r.scope)
            || !id(&self.payment_order_id)
            || !id(&self.reconciliation_case_id)
            || !orange_protocol::valid_digest(&r.target_version)
            || !matches!(self.amount.currency.as_str(), "USD" | "JPY" | "HKD" | "CNY")
            || !self
                .amount
                .minor_units
                .parse::<i64>()
                .is_ok_and(|v| v > 0 && v.to_string() == self.amount.minor_units)
        {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}
async fn row(
    tx: &mut Tx<'_>,
    scope: &Scope,
    case: &str,
) -> Result<sqlx::postgres::PgRow, AdminError> {
    sqlx::query("SELECT r.*,c.environment,c.state AS case_state,o.merchant_id FROM test_refund_orders r JOIN reconciliation_cases c USING(case_id)
        JOIN test_checkout_orders o USING(order_id) WHERE r.case_id=$1 AND c.environment='dev' AND c.owner_id=r.owner_id AND o.owner_id=r.owner_id AND ($2::text IS NULL OR r.owner_id=$2) FOR SHARE OF r,c")
        .bind(case).bind((scope.kind=="tenant").then_some(&scope.scope_id)).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)
}
pub(super) async fn target(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Refund,
) -> Result<(i64, String), AdminError> {
    p.validate(r)?;
    let v = row(tx, &r.scope, &p.reconciliation_case_id).await?;
    if v.try_get::<&str, _>("target_version")? != r.target_version
        || v.try_get::<&str, _>("order_id")? != p.payment_order_id
        || v.try_get::<&str, _>("currency")?.to_ascii_uppercase() != p.amount.currency
        || v.try_get::<i64, _>("amount_minor")?.to_string() != p.amount.minor_units
    {
        return Err(AdminError::Precondition);
    }
    if v.try_get::<&str, _>("state")? != "held"
        || matches!(v.try_get::<&str, _>("case_state")?, "resolved" | "closed")
    {
        return Err(AdminError::Conflict);
    }
    Ok((1, "held".into()))
}
pub(super) async fn authorize(
    tx: &mut Tx<'_>,
    r: &ActionRequest,
    p: &Refund,
    action: &str,
    maker: &str,
    checker: &str,
    executor: &str,
) -> Result<String, AdminError> {
    target(tx, r, p).await?;
    if maker == checker {
        return Err(AdminError::Forbidden);
    }
    let id:String=sqlx::query_scalar("UPDATE test_refund_orders SET state='authorized' WHERE case_id=$1 AND state='held' RETURNING refund_id").bind(&p.reconciliation_case_id).fetch_one(&mut **tx).await?;
    sqlx::query("INSERT INTO test_refund_authorizations(refund_id,action_id,maker_id,checker_id,executor_id,scope_kind,scope_id) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(&id).bind(action).bind(maker).bind(checker).bind(executor).bind(&r.scope.kind).bind(&r.scope.scope_id).execute(&mut **tx).await?;
    Ok(id)
}
pub(crate) async fn dispatch_authority(tx: &mut Tx<'_>, refund: &str) -> Result<i64, AdminError> {
    let a=sqlx::query("SELECT a.* FROM test_refund_authorizations a JOIN test_refund_orders r USING(refund_id) JOIN reconciliation_cases c USING(case_id)
        WHERE a.refund_id=$1 AND c.owner_id=r.owner_id AND c.environment='dev' AND c.state NOT IN ('resolved','closed') AND (a.scope_kind='platform' OR a.scope_id=r.owner_id) FOR SHARE OF c")
        .bind(refund).fetch_optional(&mut **tx).await?.ok_or(AdminError::Forbidden)?;
    let scope = Scope {
        environment: "dev".into(),
        kind: a.try_get("scope_kind")?,
        scope_id: a.try_get("scope_id")?,
    };
    let m = actor(
        tx,
        a.try_get("maker_id")?,
        &scope,
        &["actions.create", "payments.refund"],
    )
    .await?;
    let c = actor(
        tx,
        a.try_get("checker_id")?,
        &scope,
        &["actions.approve", "payments.refund.approve"],
    )
    .await?;
    let e = actor(
        tx,
        a.try_get("executor_id")?,
        &scope,
        &["actions.execute", "payments.refund"],
    )
    .await?;
    Ok(m.min(c).min(e))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    environment: String,
    scope_kind: String,
    scope_id: String,
}
pub async fn review(
    State(identity): State<Arc<AdminIdentity>>,
    Path(case): Path<String>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<Read>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, AdminError> {
    let axum::extract::Query(q) = query.map_err(|_| AdminError::Invalid)?;
    let scope = Scope {
        environment: q.environment,
        kind: q.scope_kind,
        scope_id: q.scope_id,
    };
    if !valid_scope(&scope) || !id(&case) {
        return Err(AdminError::Invalid);
    }
    let mut tx = identity.pool.begin().await?;
    let session = identity.current_in(&headers, &mut tx).await?;
    session.authorize("refunds.read", &scope)?;
    let r = row(&mut tx, &scope, &case).await?;
    let v = json!({"refund_id":r.try_get::<&str,_>("refund_id")?,"payment_order_id":r.try_get::<&str,_>("order_id")?,
        "owner_id":r.try_get::<&str,_>("owner_id")?,"reconciliation_case_id":case,"amount":{"currency":r.try_get::<&str,_>("currency")?.to_ascii_uppercase(),"minor_units":r.try_get::<i64,_>("amount_minor")?.to_string()},
        "held_microcredits":r.try_get::<i64,_>("held_units")?.to_string(),"target_version":r.try_get::<&str,_>("target_version")?,"state":r.try_get::<&str,_>("state")?});
    sqlx::query("INSERT INTO admin_audit(event_id,principal_id,event_type,scope_environment,scope_kind,scope_id,entity_id,occurred_at) VALUES($1,$2,'refund.review_read','dev',$3,$4,$5,floor(extract(epoch FROM clock_timestamp()))::bigint)")
      .bind(uuid::Uuid::new_v4().to_string()).bind(&session.view.principal_id).bind(&scope.kind).bind(&scope.scope_id).bind(case).execute(&mut *tx).await?;
    identity
        .current_in(&headers, &mut tx)
        .await?
        .authorize("refunds.read", &scope)?;
    tx.commit().await?;
    Ok(Json(v))
}
