//! Signed chargeback notices restrict only new spending on the affected account.
//! Merchant observations and platform accounting remain separate immutable facts.
use super::*;
use sqlx::{Postgres, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;

fn cleared(state: &str) -> bool {
    matches!(state, "won" | "warning_closed" | "prevented")
}
fn recognized(state: &str) -> bool {
    cleared(state)
        || matches!(
            state,
            "warning_needs_response"
                | "warning_under_review"
                | "needs_response"
                | "under_review"
                | "appeal_needs_response"
                | "appeal_under_review"
                | "lost"
        )
}
async fn source(
    tx: &mut Tx<'_>,
    merchant: &str,
    charge: &str,
) -> Result<Option<sqlx::postgres::PgRow>, Error> {
    Ok(sqlx::query("SELECT f.order_id,f.owner_id,f.amount_minor,f.currency,f.payment_intent FROM test_credit_fulfillments f JOIN test_checkout_orders o USING(order_id) WHERE f.charge_id=$1 AND o.merchant_id=$2")
        .bind(charge).bind(merchant).fetch_optional(&mut **tx).await?)
}
async fn lock_owner(tx: &mut Tx<'_>, fact: Option<&sqlx::postgres::PgRow>) -> Result<(), Error> {
    if let Some(f) = fact {
        sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
            .bind(f.try_get::<&str, _>("owner_id")?)
            .fetch_one(&mut **tx)
            .await?;
    }
    Ok(())
}
async fn policy(
    tx: &mut Tx<'_>,
    merchant: &str,
    id: &str,
    row: &sqlx::postgres::PgRow,
    fact: &sqlx::postgres::PgRow,
    block: bool,
) -> Result<(), Error> {
    let owner: &str = fact.try_get("owner_id")?;
    let order: &str = fact.try_get("order_id")?;
    if row
        .try_get::<Option<&str>, _>("owner_id")?
        .is_some_and(|v| v != owner)
        || row
            .try_get::<Option<&str>, _>("order_id")?
            .is_some_and(|v| v != order)
    {
        return Err(Error::Mismatch);
    }
    let case = if let Some(case) = row.try_get::<Option<String>, _>("case_id")? {
        case
    } else {
        let case = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at) VALUES($1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint)")
            .bind(&case).bind(owner).bind(format!("test-dispute:{id}")).execute(&mut **tx).await?;
        case
    };
    if row.try_get::<bool, _>("blocks_spend")? != block {
        sqlx::query(
            "UPDATE test_spend_policy_fences SET generation=generation+1 WHERE owner_id=$1",
        )
        .bind(owner)
        .execute(&mut **tx)
        .await?;
        crate::events::append(
            tx,
            owner,
            &case,
            if block {
                "sandbox_dispute.spending_paused"
            } else {
                "sandbox_dispute.spending_released"
            },
            id,
        )
        .await?;
    }
    sqlx::query("UPDATE test_dispute_work SET owner_id=$3,order_id=$4,case_id=$5,blocks_spend=$6 WHERE merchant_id=$1 AND dispute_id=$2").bind(merchant).bind(id).bind(owner).bind(order).bind(case).bind(block).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn notify(
    api: &Payments,
    event: &str,
    kind: &str,
    id: &str,
    charge: &str,
    hash: &str,
) -> Result<(), Error> {
    let merchant = &api.provider.merchant;
    let mut tx = api.identity.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,61))")
        .bind(format!("{merchant}:{charge}"))
        .execute(&mut *tx)
        .await?;
    let fact = source(&mut tx, merchant, charge).await?;
    lock_owner(&mut tx, fact.as_ref()).await?;
    sqlx::query("INSERT INTO test_dispute_work(merchant_id,dispute_id,charge_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(merchant).bind(id).bind(charge).execute(&mut *tx).await?;
    let w = sqlx::query(
        "SELECT * FROM test_dispute_work WHERE merchant_id=$1 AND dispute_id=$2 FOR UPDATE",
    )
    .bind(merchant)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if w.try_get::<&str, _>("charge_id")? != charge {
        return Err(Error::Conflict);
    }
    let inserted=sqlx::query("INSERT INTO test_dispute_inbox(merchant_id,event_id,dispute_id,charge_id,kind,sha256) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
        .bind(merchant).bind(event).bind(id).bind(charge).bind(kind).bind(hash).execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        let v:(String,String,String)=sqlx::query_as("SELECT dispute_id,charge_id,kind FROM test_dispute_inbox WHERE merchant_id=$1 AND event_id=$2").bind(merchant).bind(event).fetch_one(&mut *tx).await?;
        if v.0 != id || v.1 != charge || v.2 != kind {
            return Err(Error::Conflict);
        }
    } else {
        if let Some(f) = &fact {
            policy(&mut tx, merchant, id, &w, f, !cleared(w.try_get("state")?)).await?;
        }
        sqlx::query("UPDATE test_dispute_work SET notified=notified+1,next_check=clock_timestamp() WHERE merchant_id=$1 AND dispute_id=$2").bind(merchant).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
impl Payments {
    pub async fn process_dispute(&self) -> Result<bool, Error> {
        let merchant = &self.provider.merchant;
        let pool = &self.identity.pool;
        let claimed=sqlx::query("WITH ready AS (SELECT dispute_id FROM test_dispute_work WHERE merchant_id=$1 AND next_check<=clock_timestamp() AND (claim_until IS NULL OR claim_until<clock_timestamp())
           AND (notified>verified OR state NOT IN('won','warning_closed','prevented','lost')) ORDER BY next_check,dispute_id LIMIT 1 FOR UPDATE SKIP LOCKED)
           UPDATE test_dispute_work d SET generation=generation+1,claim_until=clock_timestamp()+interval '45 seconds',next_check=clock_timestamp()+interval '60 seconds'
           FROM ready WHERE d.merchant_id=$1 AND d.dispute_id=ready.dispute_id RETURNING d.dispute_id,d.charge_id,d.generation,d.notified")
           .bind(merchant).fetch_optional(pool).await?;
        let Some(c) = claimed else {
            return Ok(false);
        };
        let id: &str = c.try_get("dispute_id")?;
        let charge: &str = c.try_get("charge_id")?;
        let generation: i64 = c.try_get("generation")?;
        let notified: i64 = c.try_get("notified")?;
        let d = self.provider.dispute(id).await?;
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,61))")
            .bind(format!("{merchant}:{charge}"))
            .execute(&mut *tx)
            .await?;
        let fact = source(&mut tx, merchant, charge).await?;
        lock_owner(&mut tx, fact.as_ref()).await?;
        let current=sqlx::query("SELECT * FROM test_dispute_work WHERE merchant_id=$1 AND dispute_id=$2 AND generation=$3 AND claim_until>clock_timestamp() FOR UPDATE")
            .bind(merchant).bind(id).bind(generation).fetch_optional(&mut *tx).await?;
        let Some(w) = current else {
            return Ok(true);
        };
        let mut valid = d.charge == charge && recognized(&d.status);
        if let Some(f) = &fact {
            valid &= d.payment_intent == f.try_get::<&str, _>("payment_intent")?
                && d.currency == f.try_get::<&str, _>("currency")?
                && d.amount <= f.try_get::<i64, _>("amount_minor")?;
        }
        let state = if valid { &d.status } else { "review_required" };
        if let Some(f) = &fact {
            policy(&mut tx, merchant, id, &w, f, !cleared(state)).await?;
        }
        sqlx::query("INSERT INTO test_dispute_observations(merchant_id,dispute_id,generation,notification_version,state,amount_minor,currency,charge_id,payment_intent) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(merchant).bind(id).bind(generation).bind(notified).bind(&d.status).bind(d.amount).bind(&d.currency).bind(&d.charge).bind(&d.payment_intent).execute(&mut *tx).await?;
        sqlx::query("UPDATE test_dispute_work SET state=$3,verified=$4,claim_until=NULL,next_check=CASE WHEN notified>$4 THEN clock_timestamp() ELSE clock_timestamp()+interval '1 hour' END WHERE merchant_id=$1 AND dispute_id=$2")
            .bind(merchant).bind(id).bind(state).bind(notified).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
}
pub(super) async fn spending_status(
    State(api): State<Arc<Payments>>,
    Path(owner): Path<String>,
    h: HeaderMap,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, false).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM test_dispute_work WHERE owner_id=$1 AND blocks_spend",
    )
    .bind(&owner)
    .fetch_one(&mut **g.transaction())
    .await?;
    let v = serde_json::json!({"owner_id":owner,"positive_spending":if count==0{"available"}else{"paused"},"open_restrictions":count.to_string()});
    g.finish().await?;
    Ok(private(Json(v).into_response()))
}
