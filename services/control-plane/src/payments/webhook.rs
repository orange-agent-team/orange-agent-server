use super::*;
use axum::body::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> Result<(), Error> {
    if header.len() > 2048 {
        return Err(Error::Forbidden);
    }
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for field in header.split(',') {
        let (key, value) = field.trim().split_once('=').ok_or(Error::Forbidden)?;
        match key {
            "t" => {
                let t = value
                    .parse::<i64>()
                    .ok()
                    .filter(|t| t.to_string() == value && *t > 0)
                    .ok_or(Error::Forbidden)?;
                if timestamp.replace(t).is_some() {
                    return Err(Error::Forbidden);
                }
            }
            "v1" => {
                if let Ok(v) = hex::decode(value)
                    && v.len() == 32
                {
                    signatures.push(v);
                }
            }
            _ => {}
        }
    }
    let ts = timestamp.ok_or(Error::Forbidden)?;
    if now.abs_diff(ts) > 300 || signatures.is_empty() {
        return Err(Error::Forbidden);
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| Error::Unavailable)?;
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    if signatures
        .iter()
        .any(|v| mac.clone().verify_slice(v).is_ok())
    {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}
#[derive(Deserialize)]
struct Event {
    id: String,
    object: String,
    livemode: bool,
    account: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    data: Data,
}
#[derive(Deserialize)]
struct Data {
    object: Object,
}
#[derive(Deserialize)]
struct Object {
    metadata: Option<std::collections::BTreeMap<String, String>>,
    id: String,
    object: String,
}
pub(super) async fn receive(
    State(api): State<Arc<Payments>>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let mut values = h.get_all("stripe-signature").iter();
    let header = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Forbidden)?;
    if values.next().is_some() {
        return Err(Error::Forbidden);
    }
    verify(
        &api.provider.webhook,
        header,
        &body,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )?;
    let v: Event = serde_json::from_slice(&body).map_err(|_| Error::Invalid)?;
    if v.object != "event"
        || !stripe::identifier(&v.id, "evt_")
        || v.livemode
        || v.account
            .as_ref()
            .is_some_and(|a| a != &api.provider.merchant)
    {
        return Err(Error::Forbidden);
    }
    if matches!(
        v.kind.as_str(),
        "refund.created" | "refund.updated" | "refund.failed"
    ) {
        if v.data.object.object != "refund" || !stripe::identifier(&v.data.object.id, "re_") {
            return Err(Error::Invalid);
        }
        let Some(m) = v.data.object.metadata.as_ref() else {
            return Ok(StatusCode::NO_CONTENT.into_response());
        };
        let (Some(refund), Some(order), Some(binding)) = (
            m.get("orange_refund_id"),
            m.get("orange_order_id"),
            m.get("orange_binding"),
        ) else {
            return Ok(StatusCode::NO_CONTENT.into_response());
        };
        let mut tx = api.identity.pool.begin().await?;
        let ours=sqlx::query("SELECT r.refund_id FROM test_refund_orders r JOIN test_checkout_orders o USING(order_id) WHERE r.refund_id=$1 AND r.order_id=$2 AND o.merchant_id=$3 AND r.dispatch_binding=$4 FOR UPDATE OF r")
            .bind(refund).bind(order).bind(&api.provider.merchant).bind(binding).fetch_optional(&mut *tx).await?;
        if ours.is_some() {
            let inserted=sqlx::query("INSERT INTO test_stripe_refund_events(merchant_id,event_id,kind,refund_id,provider_refund_id,raw_sha256) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
                .bind(&api.provider.merchant).bind(&v.id).bind(&v.kind).bind(refund).bind(&v.data.object.id).bind(hex::encode(Sha256::digest(&body))).execute(&mut *tx).await?.rows_affected();
            let prior:(String,String,String)=sqlx::query_as("SELECT kind,refund_id,provider_refund_id FROM test_stripe_refund_events WHERE merchant_id=$1 AND event_id=$2").bind(&api.provider.merchant).bind(&v.id).fetch_one(&mut *tx).await?;
            if prior != (v.kind, refund.clone(), v.data.object.id) {
                return Err(Error::Conflict);
            }
            if inserted == 1 {
                sqlx::query("UPDATE test_refund_orders SET next_check=clock_timestamp() WHERE refund_id=$1 AND state IN('dispatching','pending','unknown')").bind(refund).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if !matches!(
        v.kind.as_str(),
        "checkout.session.completed"
            | "checkout.session.expired"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.async_payment_failed"
    ) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if v.data.object.object != "checkout.session"
        || !stripe::identifier(&v.data.object.id, "cs_test_")
    {
        return Err(Error::Invalid);
    }
    // A sandbox account may be shared. Metadata only routes an event to a local order.
    // The independently retrieved Session, line item, Intent and Charge determine fulfillment.
    let Some(hint) = v
        .data
        .object
        .metadata
        .as_ref()
        .and_then(|m| m.get("orange_order_id"))
        .filter(|id| valid_order(id))
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let ours:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM test_checkout_orders WHERE order_id=$1 AND merchant_id=$2 AND dispatched_at IS NOT NULL)")
       .bind(hint).bind(&api.provider.merchant).fetch_one(&api.identity.pool).await?;
    if !ours {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let mut tx = api.identity.pool.begin().await?;
    sqlx::query("INSERT INTO test_stripe_inbox(merchant_id,event_id,kind,session_id,raw_sha256) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(&api.provider.merchant).bind(&v.id).bind(&v.kind).bind(&v.data.object.id).bind(hex::encode(Sha256::digest(&body))).execute(&mut *tx).await?;
    let seen: (String, String) = sqlx::query_as(
        "SELECT kind,session_id FROM test_stripe_inbox WHERE merchant_id=$1 AND event_id=$2",
    )
    .bind(&api.provider.merchant)
    .bind(&v.id)
    .fetch_one(&mut *tx)
    .await?;
    if seen != (v.kind, v.data.object.id) {
        return Err(Error::Conflict);
    }
    sqlx::query(
        "INSERT INTO test_stripe_work(merchant_id,event_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(&api.provider.merchant)
    .bind(v.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
impl Payments {
    pub async fn process_one(&self) -> Result<bool, Error> {
        let row=sqlx::query("WITH ready AS(SELECT event_id FROM test_stripe_work WHERE merchant_id=$1 AND result IS NULL AND next_attempt<=clock_timestamp() ORDER BY next_attempt,event_id LIMIT 1 FOR UPDATE SKIP LOCKED)
            UPDATE test_stripe_work w SET attempts=attempts+1,next_attempt=clock_timestamp()+make_interval(secs=>least(3600,30*(1<<least(attempts,6)))) FROM ready r,test_stripe_inbox i
            WHERE w.merchant_id=$1 AND w.event_id=r.event_id AND i.merchant_id=w.merchant_id AND i.event_id=w.event_id RETURNING w.event_id,i.session_id")
            .bind(&self.provider.merchant).fetch_optional(&self.identity.pool).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let id: String = row.try_get("session_id")?;
        let session = self.provider.session(&id).await?;
        if session.id != id {
            return Err(Error::Mismatch);
        }
        let order = &session.metadata.orange_order_id;
        let result = if !valid_order(order) {
            "irrelevant"
        } else {
            match record(
                &self.identity.pool,
                &self.provider.merchant,
                order,
                &session,
            )
            .await
            {
                Ok(()) => match fulfill(
                    &self.identity.pool,
                    &self.provider.merchant,
                    order,
                    &session,
                )
                .await
                {
                    Ok(()) => "recorded",
                    Err(Error::Mismatch) => "review_required",
                    Err(e) => return Err(e),
                },
                Err(Error::NotFound) => "irrelevant",
                Err(Error::Mismatch) => "review_required",
                Err(e) => return Err(e),
            }
        };
        let mut tx = self.identity.pool.begin().await?;
        if result == "review_required" {
            sqlx::query("UPDATE test_checkout_orders SET state='review_required',checkout_url=NULL WHERE merchant_id=$1 AND order_id=$2 AND (session_id=$3 OR session_id IS NULL) AND state IN('open','dispatching','unknown','paid_review')")
              .bind(&self.provider.merchant).bind(order).bind(&id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE test_stripe_work SET result=$3 WHERE merchant_id=$1 AND event_id=$2 AND result IS NULL")
            .bind(&self.provider.merchant).bind(row.try_get::<&str,_>("event_id")?).bind(result).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
}
pub(super) async fn fulfill(
    pool: &PgPool,
    merchant: &str,
    order: &str,
    session: &stripe::Session,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT o.*,f.stripe_price,f.currency,f.amount_minor,f.microcredits FROM test_checkout_orders o JOIN test_credit_offers f USING(merchant_id,offer_id)
        WHERE o.order_id=$1 AND o.merchant_id=$2 FOR UPDATE OF o").bind(order).bind(merchant).fetch_one(&mut *tx).await?;
    if row.try_get::<Option<&str>, _>("session_id")? != Some(&session.id) {
        return Err(Error::Mismatch);
    }
    let (state, _) = session.validate(
        order,
        row.try_get("dispatch_binding")?,
        row.try_get("currency")?,
        row.try_get("amount_minor")?,
    )?;
    match state {
        "open" => return Err(Error::Unavailable),
        "expired" => return Ok(()),
        "paid_review" => {}
        _ => return Err(Error::Mismatch),
    }
    let (pi, ch) = session.paid_evidence(row.try_get("stripe_price")?)?;
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT payment_intent,charge_id FROM test_credit_fulfillments WHERE order_id=$1",
    )
    .bind(order)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        if existing.0 != pi || existing.1 != ch {
            return Err(Error::Mismatch);
        }
        return Ok(());
    }
    if row.try_get::<&str, _>("state")? != "paid_review" {
        return Err(Error::Mismatch);
    }
    let owner: &str = row.try_get("owner_id")?;
    let units: i64 = row.try_get("microcredits")?;
    let key = format!("test-checkout:{order}");
    let updated=sqlx::query("UPDATE wallets SET purchased=purchased+$2 WHERE owner_id=$1 AND purchased<=9223372036854775807-$2")
        .bind(owner).bind(units).execute(&mut *tx).await?;
    if updated.rows_affected() != 1 {
        return Err(Error::Unavailable);
    }
    crate::store::ledger(
        &mut tx,
        owner,
        &key,
        vec![
            (format!("{owner}:purchased:available"), units),
            (format!("stripe-test:{merchant}:clearing"), -units),
        ],
    )
    .await
    .map_err(|_| Error::Unavailable)?;
    sqlx::query("INSERT INTO test_credit_fulfillments(order_id,session_id,payment_intent,charge_id,owner_id,amount_minor,currency,microcredits,transaction_id) SELECT $1,$2,$3,$4,$5,$6,$7,$8,transaction_id FROM ledger_transactions WHERE business_key=$9")
        .bind(order).bind(&session.id).bind(pi).bind(ch).bind(owner).bind(row.try_get::<i64,_>("amount_minor")?).bind(row.try_get::<&str,_>("currency")?).bind(units).bind(key).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE test_checkout_orders SET state='credited',checkout_url=NULL WHERE order_id=$1",
    )
    .bind(order)
    .execute(&mut *tx)
    .await?;
    crate::events::append(
        &mut tx,
        owner,
        order,
        "sandbox_purchase.credited",
        row.try_get("terms_digest")?,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_signed_octets_timestamp_and_rotation() {
        let secret = "whsec_documentation_local_test";
        let payload = b"{\"id\":\"evt_fixture\"}";
        let tag = "40b94c876c99da448091962155e9e89b11dff33c0b8379f91f35c12136dd1655";
        let time = 1770000000;
        let all = format!("t={time},v1={},v1={tag}", "0".repeat(64));
        assert!(verify(secret, &all, payload, time + 300).is_ok());
        assert!(verify(secret, &all, payload, time + 301).is_err());
        assert!(verify(secret, &all, b"changed", time).is_err());
        assert!(verify(secret, &format!("{all},t={time}"), payload, time).is_err());
        assert!(verify(secret, &format!("t={time},v0={tag}"), payload, time).is_err());
    }
}
