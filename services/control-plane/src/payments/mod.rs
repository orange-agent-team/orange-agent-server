//! Sandbox purchase orders. Only provider-authenticated reconciliation may advance external payment state.
mod stripe;
mod webhook;
use crate::auth::{
    self,
    users::{UserIdentity, UserSessionGuard},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use orange_protocol::{request_digest, team::TeamRole, valid_id};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::sync::Arc;
pub use stripe::{Sandbox, SandboxConfig};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid checkout request")]
    Invalid,
    #[error("authorization denied")]
    Forbidden,
    #[error("order or offer unavailable")]
    NotFound,
    #[error("checkout identity conflict")]
    Conflict,
    #[error("provider evidence mismatch")]
    Mismatch,
    #[error("sandbox provider unavailable")]
    Unavailable,
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error("payment storage unavailable")]
    Database(#[from] sqlx::Error),
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Invalid | Self::Auth(auth::AuthError::Invalid) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "INVALID_CHECKOUT_REQUEST")
            }
            Self::Auth(auth::AuthError::Unauthenticated) => {
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED")
            }
            Self::Forbidden
            | Self::Auth(
                auth::AuthError::Forbidden | auth::AuthError::Csrf | auth::AuthError::Context,
            ) => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            Self::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Self::Conflict => (StatusCode::CONFLICT, "CHECKOUT_CONFLICT"),
            Self::Mismatch => (StatusCode::CONFLICT, "CHECKOUT_EVIDENCE_MISMATCH"),
            _ => (StatusCode::SERVICE_UNAVAILABLE, "CHECKOUT_UNAVAILABLE"),
        };
        private((status, Json(serde_json::json!({"code":code}))).into_response())
    }
}
fn private(mut r: Response) -> Response {
    r.headers_mut().insert(
        "cache-control",
        "private, no-store".parse().expect("static header"),
    );
    r
}
#[derive(Clone)]
pub struct Payments {
    identity: UserIdentity,
    provider: Sandbox,
}
impl Payments {
    pub fn new(identity: UserIdentity, provider: Sandbox) -> Self {
        Self { identity, provider }
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/v1/owners/{owner}/credit-offers", get(offers))
            .route("/api/v1/owners/{owner}/checkout-orders", post(create))
            .route(
                "/api/v1/owners/{owner}/checkout-orders/{order}",
                get(get_order),
            )
            .route(
                "/api/v1/owners/{owner}/checkout-orders/{order}/session",
                post(dispatch),
            )
            .route(
                "/api/v1/owners/{owner}/checkout-orders/{order}/cancel",
                post(cancel),
            )
            .route(
                "/api/v1/owners/{owner}/checkout-orders/{order}/reconcile",
                post(reconcile),
            )
            .route("/api/v1/payment-events/stripe-test", post(webhook::receive))
            .layer(DefaultBodyLimit::max(256 * 1024))
            .with_state(Arc::new(self.clone()))
    }
    async fn guard(
        &self,
        h: &HeaderMap,
        owner: &str,
        write: bool,
    ) -> Result<UserSessionGuard, Error> {
        let mut g = self.identity.authenticate(h).await?;
        if write {
            auth::check_write(h, &self.identity.config.user_origin, &g.view().csrf_token)?;
        }
        if !valid_id(owner) {
            return Err(Error::Invalid);
        }
        if g.view().personal_owner_id != owner {
            let user = g.view().user_id.clone();
            crate::team::organization_row(g.transaction(), owner, false)
                .await
                .map_err(|_| Error::Forbidden)?;
            let role = crate::team::membership(g.transaction(), owner, &user)
                .await
                .map_err(|_| Error::Forbidden)?;
            if !matches!(role, TeamRole::Owner | TeamRole::Billing) {
                return Err(Error::Forbidden);
            }
        }
        sqlx::query("SELECT owner_id FROM owners WHERE owner_id=$1 AND state='active' FOR SHARE")
            .bind(owner)
            .fetch_optional(&mut **g.transaction())
            .await?
            .ok_or(Error::Forbidden)?;
        Ok(g)
    }
    async fn authorized_view(
        &self,
        h: &HeaderMap,
        owner: &str,
        order: &str,
    ) -> Result<OrderView, Error> {
        let mut g = self.guard(h, owner, false).await?;
        let row = order_row(&mut g, owner, order, &self.provider.merchant).await?;
        let view = view(&row)?;
        g.finish().await?;
        Ok(view)
    }
}
fn valid_order(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == value)
}
fn key(h: &HeaderMap, id: &str) -> Result<(), Error> {
    if !valid_order(id) || h.get("idempotency-key").and_then(|s| s.to_str().ok()) != Some(id) {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}
#[derive(Serialize)]
struct Offer {
    offer_id: String,
    currency: String,
    amount_minor: String,
    microcredits: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderRequest {
    order_id: String,
    offer_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Serialize)]
struct OrderView {
    order_id: String,
    owner_id: String,
    offer: Offer,
    state: String,
    checkout_url: Option<String>,
}
fn offer(row: &sqlx::postgres::PgRow) -> Result<Offer, Error> {
    Ok(Offer {
        offer_id: row.try_get("offer_id")?,
        currency: row.try_get("currency")?,
        amount_minor: row.try_get::<i64, _>("amount_minor")?.to_string(),
        microcredits: row.try_get::<i64, _>("microcredits")?.to_string(),
    })
}
fn view(row: &sqlx::postgres::PgRow) -> Result<OrderView, Error> {
    let mut state: String = row.try_get("state")?;
    if state == "dispatching" && row.try_get::<bool, _>("abandoned")? {
        state = "unknown".into();
    }
    let url = if state == "open" {
        row.try_get("checkout_url")?
    } else {
        None
    };
    Ok(OrderView {
        order_id: row.try_get("order_id")?,
        owner_id: row.try_get("owner_id")?,
        offer: offer(row)?,
        state,
        checkout_url: url,
    })
}
async fn order_row(
    g: &mut UserSessionGuard,
    owner: &str,
    order: &str,
    merchant: &str,
) -> Result<sqlx::postgres::PgRow, Error> {
    let user = g.view().user_id.clone();
    if !valid_order(order) {
        return Err(Error::Invalid);
    }
    sqlx::query("SELECT o.*,f.currency,f.amount_minor,f.microcredits,f.stripe_price,f.enabled,coalesce(o.dispatched_at<clock_timestamp()-interval '30 seconds',false) AS abandoned
        FROM test_checkout_orders o JOIN test_credit_offers f USING(merchant_id,offer_id) WHERE o.order_id=$1 AND o.owner_id=$2 AND o.requester_id=$3 AND o.merchant_id=$4")
        .bind(order).bind(owner).bind(user).bind(merchant).fetch_optional(&mut **g.transaction()).await?.ok_or(Error::NotFound)
}
async fn offers(
    State(api): State<Arc<Payments>>,
    Path(owner): Path<String>,
    h: HeaderMap,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, false).await?;
    let rows=sqlx::query("SELECT * FROM test_credit_offers WHERE merchant_id=$1 AND enabled ORDER BY offer_id LIMIT 101").bind(&api.provider.merchant).fetch_all(&mut **g.transaction()).await?;
    if rows.len() > 100 {
        return Err(Error::Unavailable);
    }
    let result = rows.iter().map(offer).collect::<Result<Vec<_>, _>>()?;
    g.finish().await?;
    Ok(private(Json(result).into_response()))
}
async fn create(
    State(api): State<Arc<Payments>>,
    Path(owner): Path<String>,
    h: HeaderMap,
    body: Result<Json<OrderRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(req) = body.map_err(|_| Error::Invalid)?;
    key(&h, &req.order_id)?;
    if !valid_id(&req.offer_id) {
        return Err(Error::Invalid);
    }
    let user = g.view().user_id.clone();
    let merchant = &api.provider.merchant;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&req.order_id)
        .execute(&mut **g.transaction())
        .await?;
    let known: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM test_checkout_orders WHERE order_id=$1)")
            .bind(&req.order_id)
            .fetch_one(&mut **g.transaction())
            .await?;
    if !known {
        let row=sqlx::query("SELECT * FROM test_credit_offers WHERE merchant_id=$1 AND offer_id=$2 AND enabled FOR SHARE").bind(merchant).bind(&req.offer_id).fetch_optional(&mut **g.transaction()).await?.ok_or(Error::NotFound)?;
        let digest = request_digest(
            "payment.test-order.v1",
            &(
                &req.order_id,
                &owner,
                &user,
                merchant,
                offer(&row)?,
                row.try_get::<&str, _>("stripe_price")?,
            ),
        )
        .map_err(|_| Error::Invalid)?;
        sqlx::query("INSERT INTO test_checkout_orders(order_id,owner_id,requester_id,merchant_id,offer_id,terms_digest,state) VALUES($1,$2,$3,$4,$5,$6,'created')")
           .bind(&req.order_id).bind(&owner).bind(user).bind(merchant).bind(&req.offer_id).bind(digest).execute(&mut **g.transaction()).await?;
    }
    let result = view(&order_row(&mut g, &owner, &req.order_id, merchant).await?)?;
    if result.offer.offer_id != req.offer_id {
        return Err(Error::Conflict);
    }
    g.finish().await?;
    Ok(private((StatusCode::CREATED, Json(result)).into_response()))
}
async fn get_order(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Response, Error> {
    Ok(private(
        Json(api.authorized_view(&h, &owner, &order).await?).into_response(),
    ))
}
async fn dispatch(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Empty>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(Empty {}) = body.map_err(|_| Error::Invalid)?;
    key(&h, &order)?;
    let row = order_row(&mut g, &owner, &order, &api.provider.merchant).await?;
    if row.try_get::<&str, _>("state")? != "created" {
        let current = view(&row)?;
        g.finish().await?;
        return Ok(private(Json(current).into_response()));
    }
    if !row.try_get::<bool, _>("enabled")? {
        return Err(Error::Conflict);
    }
    g.finish().await?;
    // The account lookup is read-only. Failures at this point leave the order safe to retry.
    api.provider.account().await?;
    let mut g = api.guard(&h, &owner, true).await?;
    let row = order_row(&mut g, &owner, &order, &api.provider.merchant).await?;
    let price: String = row.try_get("stripe_price")?;
    let binding = uuid::Uuid::new_v4().simple().to_string();
    let claimed=sqlx::query("UPDATE test_checkout_orders o SET state='dispatching',dispatched_at=clock_timestamp(),dispatch_binding=$2 WHERE o.order_id=$1 AND o.state='created'
         AND EXISTS(SELECT 1 FROM test_credit_offers f WHERE f.merchant_id=o.merchant_id AND f.offer_id=o.offer_id AND f.enabled FOR SHARE)")
         .bind(&order).bind(&binding).execute(&mut **g.transaction()).await?.rows_affected()==1;
    if !claimed
        && order_row(&mut g, &owner, &order, &api.provider.merchant)
            .await?
            .try_get::<&str, _>("state")?
            == "created"
    {
        return Err(Error::Conflict);
    }
    g.finish().await?;
    if claimed {
        // A transport error can follow a successful remote creation. Never issue a second POST.
        let response = api
            .provider
            .create(&order, &price, &binding, &api.identity.config.user_origin)
            .await;
        match response {
            Ok(session) => {
                if let Err(e) =
                    record(&api.identity.pool, &api.provider.merchant, &order, &session).await
                {
                    if matches!(e, Error::Mismatch) {
                        mark_unknown(&api.identity.pool, &order).await?;
                    }
                    return Err(e);
                }
            }
            Err(_) => {
                mark_unknown(&api.identity.pool, &order).await?;
            }
        }
    }
    Ok(private(
        Json(api.authorized_view(&h, &owner, &order).await?).into_response(),
    ))
}
async fn cancel(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Empty>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(Empty {}) = body.map_err(|_| Error::Invalid)?;
    key(&h, &order)?;
    order_row(&mut g, &owner, &order, &api.provider.merchant).await?;
    let changed=sqlx::query("UPDATE test_checkout_orders SET state='cancelled' WHERE order_id=$1 AND state IN('created','cancelled')")
        .bind(&order).execute(&mut **g.transaction()).await?.rows_affected();
    if changed != 1 {
        return Err(Error::Conflict);
    }
    let result = view(&order_row(&mut g, &owner, &order, &api.provider.merchant).await?)?;
    g.finish().await?;
    Ok(private(Json(result).into_response()))
}
async fn reconcile(
    State(api): State<Arc<Payments>>,
    Path((owner, order)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Empty>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, Error> {
    let mut g = api.guard(&h, &owner, true).await?;
    let Json(Empty {}) = body.map_err(|_| Error::Invalid)?;
    key(&h, &order)?;
    let row = order_row(&mut g, &owner, &order, &api.provider.merchant).await?;
    let maybe: Option<String> = row.try_get("session_id")?;
    g.finish().await?;
    if let Some(id) = maybe {
        let s = api.provider.session(&id).await?;
        if s.id != id {
            return Err(Error::Mismatch);
        }
        record(&api.identity.pool, &api.provider.merchant, &order, &s).await?;
        if s.is_complete() {
            webhook::fulfill(&api.identity.pool, &api.provider.merchant, &order, &s).await?;
        }
    }
    Ok(private(
        Json(api.authorized_view(&h, &owner, &order).await?).into_response(),
    ))
}
async fn mark_unknown(pool: &PgPool, order: &str) -> Result<(), Error> {
    sqlx::query(
        "UPDATE test_checkout_orders SET state='unknown' WHERE order_id=$1 AND state='dispatching'",
    )
    .bind(order)
    .execute(pool)
    .await?;
    Ok(())
}
async fn record(
    pool: &PgPool,
    merchant: &str,
    order: &str,
    s: &stripe::Session,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT o.state,o.session_id,o.dispatch_binding,f.currency,f.amount_minor FROM test_checkout_orders o JOIN test_credit_offers f USING(merchant_id,offer_id)
        WHERE o.order_id=$1 AND o.merchant_id=$2 FOR UPDATE OF o").bind(order).bind(merchant).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound)?;
    let old: &str = row.try_get("state")?;
    let (new, url) = s.validate(
        order,
        row.try_get("dispatch_binding")?,
        row.try_get("currency")?,
        row.try_get("amount_minor")?,
    )?;
    if matches!(old, "created" | "cancelled")
        || row
            .try_get::<Option<&str>, _>("session_id")?
            .is_some_and(|id| id != s.id)
    {
        return Err(Error::Mismatch);
    }
    if matches!(old, "created" | "dispatching" | "unknown" | "open") || old == new {
        sqlx::query("UPDATE test_checkout_orders SET state=$2,session_id=$3,checkout_url=$4 WHERE order_id=$1")
            .bind(order).bind(new).bind(&s.id).bind(url).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
