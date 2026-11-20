use super::stripe::{RefundObject, RefundTerms};
use super::*;
enum Work {
    Create,
    Read(String),
    Hint(String),
    Find(Option<String>),
}
fn terms(r: &sqlx::postgres::PgRow) -> Result<RefundTerms, Error> {
    Ok(RefundTerms {
        refund: r.try_get("refund_id")?,
        order: r.try_get("order_id")?,
        currency: r.try_get("currency")?,
        amount: r.try_get("amount_minor")?,
        original_amount: r.try_get("original_amount")?,
        prior: r.try_get("prior_minor")?,
        charge: r.try_get("charge_id")?,
        intent: r.try_get("payment_intent")?,
        binding: r
            .try_get::<Option<String>, _>("dispatch_binding")?
            .unwrap_or_default(),
    })
}
impl Payments {
    pub async fn process_refund(&self) -> Result<bool, Error> {
        let pool = &self.identity.pool;
        let merchant = &self.provider.merchant;
        let mut tx = pool.begin().await?;
        let r=sqlx::query("SELECT r.*,f.charge_id,f.payment_intent,f.amount_minor AS original_amount,(SELECT e.provider_refund_id FROM test_stripe_refund_events e WHERE e.refund_id=r.refund_id ORDER BY e.received_at DESC,e.event_id DESC LIMIT 1) AS event_hint FROM test_refund_orders r JOIN test_credit_fulfillments f USING(order_id)
            JOIN test_checkout_orders o USING(order_id) WHERE o.merchant_id=$1 AND r.next_check<=clock_timestamp() AND r.state IN('authorized','dispatching','pending','unknown')
            ORDER BY r.next_check,r.refund_id LIMIT 1 FOR UPDATE OF r SKIP LOCKED")
            .bind(merchant).fetch_optional(&mut *tx).await?;
        let Some(r) = r else {
            return Ok(false);
        };
        let mut t = terms(&r)?;
        let current: &str = r.try_get("state")?;
        let owner: String = r.try_get("owner_id")?;
        let work = if current == "authorized" {
            Work::Create
        } else if let Some(id) = r.try_get::<Option<String>, _>("provider_refund_id")? {
            Work::Read(id)
        } else {
            if let Some(id) = r.try_get::<Option<String>, _>("event_hint")? {
                Work::Hint(id)
            } else {
                Work::Find(r.try_get("scan_after")?)
            }
        };
        // This lease only schedules work. It is not permission to send a refund.
        sqlx::query("UPDATE test_refund_orders SET checks=checks+1,next_check=clock_timestamp()+make_interval(secs=>least(3600,30*(1<<least(checks,7)))) WHERE refund_id=$1")
            .bind(&t.refund).execute(&mut *tx).await?;
        tx.commit().await?;
        match work {
            Work::Create => {
                if let Err(e) = self.provider.check_refundable_charge(&t).await {
                    if matches!(e, Error::Mismatch) {
                        flag(pool, &t.refund, "authorized", "review_required").await?;
                    }
                    return Err(e);
                }
                let mut tx = pool.begin().await?;
                sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
                    .bind(&owner)
                    .fetch_one(&mut *tx)
                    .await?;
                let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM test_dispute_work WHERE order_id=$1 AND blocks_spend)").bind(&t.order).fetch_one(&mut *tx).await?;
                if blocked {
                    return Ok(true);
                }
                let state: String = sqlx::query_scalar(
                    "SELECT state FROM test_refund_orders WHERE refund_id=$1 FOR UPDATE",
                )
                .bind(&t.refund)
                .fetch_one(&mut *tx)
                .await?;
                if state != "authorized" {
                    return Ok(true);
                }
                let deadline = match crate::admin::actions::refunds::dispatch_authority(
                    &mut tx, &t.refund,
                )
                .await
                {
                    Ok(deadline) => deadline,
                    Err(
                        crate::admin::AdminError::Forbidden | crate::admin::AdminError::NotFound,
                    ) => {
                        sqlx::query("UPDATE test_refund_orders SET state='approval_lost' WHERE refund_id=$1").bind(&t.refund).execute(&mut *tx).await?;
                        tx.commit().await?;
                        return Ok(true);
                    }
                    Err(_) => return Err(Error::Unavailable),
                };
                t.binding = uuid::Uuid::new_v4().simple().to_string();
                let claimed=sqlx::query("UPDATE test_refund_orders SET state='dispatching',dispatch_binding=$2,next_check=clock_timestamp()+interval '60 seconds' WHERE refund_id=$1 AND state='authorized' AND $3>floor(extract(epoch FROM clock_timestamp()))::bigint")
                    .bind(&t.refund).bind(&t.binding).bind(deadline).execute(&mut *tx).await?.rows_affected();
                tx.commit().await?;
                if claimed != 1 {
                    return Ok(true);
                }
                match self.provider.create_refund(&t).await {
                    Ok(r) if r.matches(&t) => {
                        bind_remote(pool, &t, &r).await?;
                    }
                    Ok(_) => {
                        flag(pool, &t.refund, "dispatching", "review_required").await?;
                    }
                    Err(_) => {
                        flag(pool, &t.refund, "dispatching", "unknown").await?;
                    }
                }
            }
            Work::Find(cursor) => {
                let (found, next) = self.provider.find_refund(&t, cursor.as_deref()).await?;
                if let Some(remote) = found {
                    if !remote.matches(&t) {
                        flag(pool, &t.refund, current, "review_required").await?;
                        return Ok(true);
                    }
                    bind_remote(pool, &t, &remote).await?;
                } else {
                    sqlx::query("UPDATE test_refund_orders SET scan_after=$2,state='unknown' WHERE refund_id=$1 AND state IN('dispatching','unknown') AND provider_refund_id IS NULL")
                    .bind(&t.refund).bind(next).execute(pool).await?;
                }
            }
            Work::Hint(id) => {
                let v = self.provider.refund(&id).await?;
                if v.matches(&t) {
                    bind_remote(pool, &t, &v).await?;
                } else {
                    flag(pool, &t.refund, current, "review_required").await?;
                }
            }
            Work::Read(id) => {
                let r = self.provider.refund(&id).await?;
                if !r.matches(&t) {
                    flag(pool, &t.refund, current, "review_required").await?;
                    return Ok(true);
                }
                let end = match r.state() {
                    "pending" | "requires_action" => None,
                    "succeeded" => Some("succeeded"),
                    "failed" | "canceled" => Some("failed"),
                    _ => {
                        flag(pool, &t.refund, current, "review_required").await?;
                        return Ok(true);
                    }
                };
                if let Some(outcome) = end {
                    let mut tx = pool.begin().await?;
                    refunds::finalize(&mut tx, &owner, &t.refund, outcome, Some(&id)).await?;
                    tx.commit().await?;
                }
            }
        }
        Ok(true)
    }
}
async fn flag(pool: &PgPool, id: &str, from: &str, to: &str) -> Result<(), Error> {
    sqlx::query("UPDATE test_refund_orders SET state=$3 WHERE refund_id=$1 AND state=$2")
        .bind(id)
        .bind(from)
        .bind(to)
        .execute(pool)
        .await?;
    Ok(())
}
async fn bind_remote(pool: &PgPool, t: &RefundTerms, r: &RefundObject) -> Result<(), Error> {
    if !r.matches(t) {
        return Err(Error::Mismatch);
    }
    let changed=sqlx::query("UPDATE test_refund_orders SET provider_refund_id=$2,state='pending',scan_after=NULL,checks=0,next_check=clock_timestamp()
      WHERE refund_id=$1 AND state IN('unknown','dispatching','pending') AND dispatch_binding=$3 AND (provider_refund_id IS NULL OR provider_refund_id=$2)")
      .bind(&t.refund).bind(&r.id).bind(&t.binding).execute(pool).await?.rows_affected();
    if changed != 1 {
        let prior:Option<String>=sqlx::query_scalar("SELECT provider_refund_id FROM test_refund_orders WHERE refund_id=$1 AND state IN('succeeded','failed')").bind(&t.refund).fetch_optional(pool).await?.flatten();
        if prior.as_deref() != Some(r.id.as_str()) {
            return Err(Error::Conflict);
        }
    }
    Ok(())
}
