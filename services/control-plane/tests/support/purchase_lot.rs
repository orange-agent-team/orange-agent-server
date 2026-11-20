//! Explicit historical funding fixture, independent of current provider and identity credentials.
use uuid::Uuid;
pub async fn credited(
    pool: &sqlx::PgPool,
    owner: &str,
    user: &str,
    merchant: &str,
    sku: &str,
    units: i64,
) -> String {
    let order = Uuid::new_v4().to_string();
    let compact = order.replace('-', "");
    let session = format!("cs_test_{compact}");
    let transaction = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await.expect("historic purchase transaction");
    sqlx::query("INSERT INTO test_credit_offers VALUES($1,$2,$3,'usd',500,$4,true)")
        .bind(merchant)
        .bind(sku)
        .bind(format!("price_{compact}"))
        .bind(units)
        .execute(&mut *tx)
        .await
        .expect("historic price");
    sqlx::query("INSERT INTO test_checkout_orders(order_id,owner_id,requester_id,merchant_id,offer_id,terms_digest,state,dispatch_binding,session_id,dispatched_at) VALUES($1,$2,$3,$4,$5,$6,'credited',$7,$8,clock_timestamp())")
        .bind(&order).bind(owner).bind(user).bind(merchant).bind(sku).bind("c".repeat(64)).bind(&compact).bind(&session).execute(&mut *tx).await.expect("historic order");
    sqlx::query("UPDATE wallets SET purchased=purchased+$2 WHERE owner_id=$1")
        .bind(owner)
        .bind(units)
        .execute(&mut *tx)
        .await
        .expect("historic balance");
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&transaction)
    .bind(format!("test-checkout:{order}"))
    .bind(owner)
    .execute(&mut *tx)
    .await
    .expect("historic transaction");
    sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,$2,$3),($1,'stripe-test:clearing',-$3)").bind(&transaction).bind(format!("{}:purchased:available",owner)).bind(units).execute(&mut *tx).await.expect("balanced entries");
    sqlx::query("INSERT INTO test_credit_fulfillments(order_id,session_id,payment_intent,charge_id,owner_id,amount_minor,currency,microcredits,transaction_id) VALUES($1,$2,$3,$4,$5,500,'usd',$6,$7)")
        .bind(&order).bind(session).bind(format!("pi_{compact}")).bind(format!("ch_{compact}")).bind(owner).bind(units).bind(transaction).execute(&mut *tx).await.expect("historic fulfillment");
    tx.commit().await.expect("historic state");
    order
}
