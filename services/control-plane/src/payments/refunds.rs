//! Reserve only the unused part of the original purchased lot. A local hold has no merchant effect.
use super::*;
use sqlx::{Postgres, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Apply {
    refund_id: String,
    currency: String,
    amount_minor: String,
}
#[derive(Serialize)]
struct RefundView {
    refund_id: String,
    order_id: String,
    owner_id: String,
    currency: String,
    amount_minor: String,
    held_microcredits: String,
    case_id: String,
    state: String,
}
fn projection(row: &sqlx::postgres::PgRow) -> Result<RefundView, Error> {
    Ok(RefundView {
        refund_id: row.try_get("refund_id")?,
        order_id: row.try_get("order_id")?,
        owner_id: row.try_get("owner_id")?,
        currency: row.try_get("currency")?,
        amount_minor: row.try_get::<i64, _>("amount_minor")?.to_string(),
        held_microcredits: row.try_get::<i64, _>("held_units")?.to_string(),
        case_id: row.try_get("case_id")?,
        state: row.try_get("state")?,
    })
}
fn cumulative(minor: i64, units: i64, total: i64) -> Result<i64, Error> {
    if minor < 0 || units <= 0 || total <= 0 || minor > total {
        return Err(Error::Invalid);
    }
    let v = (minor as u128 * units as u128).div_ceil(total as u128);
    i64::try_from(v).map_err(|_| Error::Invalid)
}
fn limit_and_hold(
    original: i64,
    total: i64,
    prior: i64,
    available: i64,
    request: i64,
) -> Result<(i64, i64), Error> {
    let used = cumulative(prior, original, total)?;
    if available < 0 || i128::from(available) + i128::from(used) > i128::from(original) {
        return Err(Error::Mismatch);
    }
    let eligible = i64::try_from(
        (i128::from(available) + i128::from(used)) * i128::from(total) / i128::from(original),
    )
    .map_err(|_| Error::Mismatch)?
        - prior;
    if request < 0 || request > eligible {
        return Err(Error::Conflict);
    }
    let hold = cumulative(prior + request, original, total)? - used;
    if request > 0 && hold <= 0 {
        return Err(Error::Invalid);
    }
    Ok((eligible, hold))
}
async fn wallet_lock(tx: &mut Tx<'_>, owner: &str) -> Result<(), Error> {
    sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(())
}
async fn lot_row(
    tx: &mut Tx<'_>,
    merchant: &str,
    owner: &str,
    order: &str,
) -> Result<sqlx::postgres::PgRow, Error> {
    sqlx::query("SELECT l.*,f.amount_minor,f.currency,f.charge_id,EXISTS(SELECT 1 FROM test_dispute_work d WHERE d.order_id=l.order_id AND d.blocks_spend) AS disputed,COALESCE((SELECT sum(r.amount_minor)::bigint FROM test_refund_orders r WHERE r.order_id=l.order_id AND r.state='succeeded'),0) AS prior_minor,
        (SELECT r.refund_id FROM test_refund_orders r WHERE r.order_id=l.order_id AND r.state NOT IN ('succeeded','failed','cancelled')) AS pending
        FROM test_purchase_lots l JOIN test_credit_fulfillments f USING(order_id) JOIN test_checkout_orders o USING(order_id)
        WHERE l.order_id=$1 AND l.owner_id=$2 AND f.owner_id=l.owner_id AND o.owner_id=l.owner_id AND o.merchant_id=$3 AND o.state='credited' FOR UPDATE OF l")
        .bind(order).bind(owner).bind(merchant).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)
}
pub(super) async fn eligibility(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, false).await?;
    let r = lot_row(g.transaction(), &api.provider.merchant, &owner, &order).await?;
    let pending: Option<String> = r.try_get("pending")?;
    let (maximum, _) = limit_and_hold(
        r.try_get("original_units")?,
        r.try_get("amount_minor")?,
        r.try_get("prior_minor")?,
        r.try_get("available")?,
        0,
    )?;
    let v = serde_json::json!({"order_id":order,"owner_id":owner,"currency":r.try_get::<&str,_>("currency")?,
        "max_refundable_minor":if pending.is_some()||r.try_get::<bool,_>("disputed")?{0}else{maximum}.to_string(),"unspent_microcredits":r.try_get::<i64,_>("available")?.to_string(),"pending_refund_id":pending});
    g.finish().await?;
    Ok(private(Json(v).into_response()))
}
async fn existing(
    tx: &mut Tx<'_>,
    owner: &str,
    id: &str,
    merchant: &str,
) -> Result<Option<sqlx::postgres::PgRow>, Error> {
    Ok(sqlx::query("SELECT r.* FROM test_refund_orders r JOIN test_checkout_orders o USING(order_id) WHERE r.refund_id=$1 AND r.owner_id=$2 AND o.merchant_id=$3")
        .bind(id).bind(owner).bind(merchant).fetch_optional(&mut **tx).await?)
}
pub(super) async fn apply(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Apply>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(req) = body.map_err(|_| Error::Invalid)?;
    key(&h, &req.refund_id)?;
    let amount = req
        .amount_minor
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == req.amount_minor)
        .ok_or(Error::Invalid)?;
    let user = g.view().user_id.clone();
    let merchant = &api.provider.merchant;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&req.refund_id)
        .execute(&mut **g.transaction())
        .await?;
    wallet_lock(g.transaction(), &owner).await?;
    if let Some(prior) = existing(g.transaction(), &owner, &req.refund_id, merchant).await? {
        if prior.try_get::<&str, _>("requester_id")? != user
            || prior.try_get::<&str, _>("order_id")? != order
            || prior.try_get::<&str, _>("currency")? != req.currency
            || prior.try_get::<i64, _>("amount_minor")? != amount
        {
            return Err(Error::Conflict);
        }
        let v = projection(&prior)?;
        g.finish().await?;
        return Ok(private((StatusCode::CREATED, Json(v)).into_response()));
    }
    let r = lot_row(g.transaction(), merchant, &owner, &order).await?;
    if r.try_get::<Option<String>, _>("pending")?.is_some()
        || r.try_get::<&str, _>("currency")? != req.currency
    {
        return Err(Error::Conflict);
    }
    let disputed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM test_dispute_work WHERE order_id=$1 AND blocks_spend)",
    )
    .bind(&order)
    .fetch_one(&mut **g.transaction())
    .await?;
    if disputed {
        return Err(Error::SpendingPaused);
    }
    let prior = r.try_get::<i64, _>("prior_minor")?;
    let (_, hold) = limit_and_hold(
        r.try_get("original_units")?,
        r.try_get("amount_minor")?,
        prior,
        r.try_get("available")?,
        amount,
    )?;
    let case_id = uuid::Uuid::new_v4().to_string();
    let digest = request_digest(
        "payment.test-refund.v1",
        &(
            &req.refund_id,
            merchant,
            &owner,
            &user,
            &order,
            &case_id,
            &req.currency,
            amount,
            hold,
            prior,
            r.try_get::<&str, _>("charge_id")?,
        ),
    )
    .map_err(|_| Error::Invalid)?;
    sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at) VALUES($1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint)")
       .bind(&case_id).bind(&owner).bind(format!("test-refund:{}",req.refund_id)).execute(&mut **g.transaction()).await?;
    sqlx::query("INSERT INTO test_refund_orders(refund_id,order_id,owner_id,requester_id,case_id,currency,amount_minor,held_units,prior_minor,target_version,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'held')")
        .bind(&req.refund_id).bind(&order).bind(&owner).bind(&user).bind(case_id).bind(&req.currency).bind(amount).bind(hold).bind(prior).bind(&digest).execute(&mut **g.transaction()).await?;
    let changed=sqlx::query("UPDATE wallets SET purchased=purchased-$2,reserved_purchased=reserved_purchased+$2 WHERE owner_id=$1 AND purchased>=$2")
        .bind(&owner).bind(hold).execute(&mut **g.transaction()).await?.rows_affected();
    if changed != 1 {
        return Err(Error::Conflict);
    }
    sqlx::query("UPDATE test_purchase_lots SET available=available-$2,refund_held=refund_held+$2 WHERE order_id=$1").bind(&order).bind(hold).execute(&mut **g.transaction()).await?;
    crate::store::ledger(
        g.transaction(),
        &owner,
        &format!("test-refund-hold:{}", req.refund_id),
        vec![
            (format!("{owner}:purchased:available"), -hold),
            (format!("{owner}:purchased:refund-held"), hold),
        ],
    )
    .await
    .map_err(|_| Error::Unavailable)?;
    crate::events::append(
        g.transaction(),
        &owner,
        &req.refund_id,
        "sandbox_refund.held",
        &digest,
    )
    .await?;
    let v = projection(
        &existing(g.transaction(), &owner, &req.refund_id, merchant)
            .await?
            .ok_or(Error::Mismatch)?,
    )?;
    g.finish().await?;
    Ok(private((StatusCode::CREATED, Json(v)).into_response()))
}
pub(super) async fn get_refund(
    State(api): State<Arc<Payments>>,
    Path((owner, id)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, false).await?;
    let v = projection(
        &existing(g.transaction(), &owner, &id, &api.provider.merchant)
            .await?
            .ok_or(Error::NotFound)?,
    )?;
    g.finish().await?;
    Ok(private(Json(v).into_response()))
}
pub(super) async fn cancel_refund(
    State(api): State<Arc<Payments>>,
    Path((owner, id)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Empty>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(Empty {}) = body.map_err(|_| Error::Invalid)?;
    key(&h, &id)?;
    let r = existing(g.transaction(), &owner, &id, &api.provider.merchant)
        .await?
        .ok_or(Error::NotFound)?;
    if r.try_get::<&str, _>("requester_id")? != g.view().user_id {
        return Err(Error::Forbidden);
    }
    finalize(g.transaction(), &owner, &id, "cancelled", None).await?;
    let v = projection(
        &existing(g.transaction(), &owner, &id, &api.provider.merchant)
            .await?
            .ok_or(Error::NotFound)?,
    )?;
    g.finish().await?;
    Ok(private(Json(v).into_response()))
}
pub(super) async fn finalize(
    tx: &mut Tx<'_>,
    owner: &str,
    id: &str,
    outcome: &str,
    remote: Option<&str>,
) -> Result<(), Error> {
    wallet_lock(tx, owner).await?;
    let row=sqlx::query("SELECT r.*,o.merchant_id FROM test_refund_orders r JOIN test_checkout_orders o USING(order_id) WHERE refund_id=$1 AND r.owner_id=$2 FOR UPDATE OF r")
        .bind(id).bind(owner).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound)?;
    let old: &str = row.try_get("state")?;
    if let Some(prior) = sqlx::query_scalar::<_, String>(
        "SELECT outcome FROM test_refund_receipts WHERE refund_id=$1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    {
        return if prior == outcome
            && remote == row.try_get::<Option<&str>, _>("provider_refund_id")?
        {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    let success = match outcome {
        "cancelled"
            if remote.is_none()
                && (matches!(old, "held" | "authorized" | "approval_lost")
                    || (old == "review_required"
                        && row
                            .try_get::<Option<String>, _>("dispatch_binding")?
                            .is_none())) =>
        {
            false
        }
        "succeeded" | "failed"
            if matches!(
                old,
                "dispatching" | "unknown" | "pending" | "review_required"
            ) && remote.is_some()
                && remote == row.try_get::<Option<&str>, _>("provider_refund_id")? =>
        {
            outcome == "succeeded"
        }
        _ => return Err(Error::Conflict),
    };
    let units: i64 = row.try_get("held_units")?;
    let order: &str = row.try_get("order_id")?;
    let returned = if success { 0 } else { units };
    sqlx::query("UPDATE wallets SET reserved_purchased=reserved_purchased-$2,purchased=purchased+$3 WHERE owner_id=$1")
        .bind(owner).bind(units).bind(returned).execute(&mut **tx).await?;
    sqlx::query("UPDATE test_purchase_lots SET refund_held=refund_held-$2,available=available+$3,refunded=refunded+($2-$3) WHERE order_id=$1")
        .bind(order).bind(units).bind(returned).execute(&mut **tx).await?;
    let account = if success {
        format!(
            "stripe-test:{}:clearing",
            row.try_get::<&str, _>("merchant_id")?
        )
    } else {
        format!("{owner}:purchased:available")
    };
    let key = format!("test-refund-end:{id}");
    crate::store::ledger(
        tx,
        owner,
        &key,
        vec![
            (format!("{owner}:purchased:refund-held"), -units),
            (account, units),
        ],
    )
    .await
    .map_err(|_| Error::Unavailable)?;
    sqlx::query("INSERT INTO test_refund_receipts(refund_id,outcome,transaction_id) SELECT $1,$2,transaction_id FROM ledger_transactions WHERE business_key=$3")
        .bind(id).bind(outcome).bind(&key).execute(&mut **tx).await?;
    sqlx::query("UPDATE test_refund_orders SET state=$2 WHERE refund_id=$1")
        .bind(id)
        .bind(outcome)
        .execute(&mut **tx)
        .await?;
    crate::events::append(
        tx,
        owner,
        id,
        "sandbox_refund.finalized",
        row.try_get("target_version")?,
    )
    .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_partial_refunds_preserve_the_original_credit_total() {
        assert_eq!(limit_and_hold(3, 2, 0, 3, 1).expect("first"), (2, 2));
        assert_eq!(limit_and_hold(3, 2, 1, 1, 1).expect("rest"), (1, 1));
        assert!(limit_and_hold(9000, 500, 0, 17, 1).is_err());
        assert_eq!(
            limit_and_hold(9000, 500, 100, 3600, 100).expect("partial"),
            (200, 1800)
        );
    }
}
