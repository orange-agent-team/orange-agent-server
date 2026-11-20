//! Stripe API v1 sandbox transport. Sensitive configuration and hosted URLs never enter diagnostic output.
use super::Error;
use reqwest::{Client, Response, Url, redirect::Policy};
use serde::Deserialize;
use std::time::Duration;

pub const VERSION: &str = "2026-11-18.endive";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub merchant_id: String,
    pub api_key: String,
    pub webhook_secret: String,
}
#[derive(Clone)]
pub struct Sandbox {
    pub(super) merchant: String,
    pub(super) webhook: std::sync::Arc<str>,
    key: std::sync::Arc<str>,
    client: Client,
    base: Url,
}
pub(super) fn identifier(value: &str, prefix: &str) -> bool {
    value.len() <= 180
        && value.strip_prefix(prefix).is_some_and(|s| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}
impl Sandbox {
    #[cfg(unix)]
    pub fn load(path: &std::path::Path) -> Result<Self, Error> {
        let bytes = crate::connector_file::read_private(path).map_err(|_| Error::Invalid)?;
        let config = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
        Self::new(
            config,
            Url::parse("https://api.stripe.com/v1/").map_err(|_| Error::Invalid)?,
        )
    }
    #[cfg(feature = "integration-tests")]
    pub fn loopback_fixture(
        config: SandboxConfig,
        addr: std::net::SocketAddr,
    ) -> Result<Self, Error> {
        if !addr.ip().is_loopback() || addr.port() == 0 {
            return Err(Error::Invalid);
        }
        Self::new(
            config,
            Url::parse(&format!("http://{addr}/v1/")).map_err(|_| Error::Invalid)?,
        )
    }
    fn new(config: SandboxConfig, base: Url) -> Result<Self, Error> {
        if !identifier(&config.merchant_id, "acct_")
            || !identifier(&config.api_key, "sk_test_")
            || config.api_key.len() < 18
            || !identifier(&config.webhook_secret, "whsec_")
            || config.webhook_secret.len() < 22
        {
            return Err(Error::Invalid);
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(4))
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            merchant: config.merchant_id,
            webhook: config.webhook_secret.into(),
            key: config.api_key.into(),
            client,
            base,
        })
    }
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let url = self.base.join(path).map_err(|_| Error::Invalid)?;
        Ok(self
            .client
            .request(method, url)
            .basic_auth(self.key.as_ref(), None::<&str>)
            .header("Stripe-Version", VERSION))
    }
    pub(super) async fn account(&self) -> Result<(), Error> {
        #[derive(Deserialize)]
        struct Account {
            id: String,
            object: String,
        }
        let v: Account = receive(
            self.request(reqwest::Method::GET, "account")?
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await?;
        if v.object != "account" || v.id != self.merchant {
            return Err(Error::Mismatch);
        }
        Ok(())
    }
    pub(super) async fn create(
        &self,
        order: &str,
        price: &str,
        binding: &str,
        origin: &str,
    ) -> Result<Session, Error> {
        let done = format!("{origin}/account?checkout_order={order}");
        let form = [
            ("mode", "payment"),
            ("payment_method_types[0]", "card"),
            ("line_items[0][price]", price),
            ("line_items[0][quantity]", "1"),
            ("success_url", &done),
            ("cancel_url", &done),
            ("client_reference_id", order),
            ("metadata[orange_order_id]", order),
            ("metadata[orange_binding]", binding),
            ("payment_intent_data[metadata][orange_binding]", binding),
            ("payment_intent_data[metadata][orange_order_id]", order),
        ];
        receive(
            self.request(reqwest::Method::POST, "checkout/sessions")?
                .header("Idempotency-Key", format!("orange-checkout-{order}"))
                .form(&form)
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await
    }
    pub(super) async fn session(&self, id: &str) -> Result<Session, Error> {
        if !identifier(id, "cs_test_") {
            return Err(Error::Mismatch);
        }
        self.account().await?;
        receive(
            self.request(reqwest::Method::GET, &format!("checkout/sessions/{id}"))?
                .query(&[
                    ("expand[]", "line_items"),
                    ("expand[]", "payment_intent.latest_charge"),
                ])
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await
    }
}

async fn receive<T: serde::de::DeserializeOwned>(mut response: Response) -> Result<T, Error> {
    if !response.status().is_success() || response.content_length().is_some_and(|v| v > 262144) {
        return Err(Error::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
        if chunk.len() > 262144 - bytes.len() {
            return Err(Error::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Mismatch)
}
#[derive(Deserialize)]
pub(super) struct Metadata {
    pub orange_order_id: String,
    orange_binding: String,
}
#[derive(Deserialize)]
pub(super) struct Session {
    pub id: String,
    object: String,
    livemode: bool,
    mode: String,
    client_reference_id: String,
    pub metadata: Metadata,
    line_items: Option<Items>,
    payment_intent: Option<IntentRef>,
    currency: String,
    amount_total: i64,
    payment_status: String,
    status: String,
    url: Option<String>,
    payment_method_types: Vec<String>,
}
impl Session {
    pub(super) fn is_complete(&self) -> bool {
        self.status == "complete"
    }
    pub(super) fn validate(
        &self,
        order: &str,
        binding: &str,
        currency: &str,
        minor: i64,
    ) -> Result<(&'static str, Option<&str>), Error> {
        if self.object != "checkout.session"
            || !identifier(&self.id, "cs_test_")
            || self.livemode
            || self.mode != "payment"
            || self.client_reference_id != order
            || self.metadata.orange_binding != binding
            || self.metadata.orange_order_id != order
            || self.currency != currency
            || self.amount_total != minor
            || minor <= 0
            || self.payment_method_types != ["card"]
        {
            return Err(Error::Mismatch);
        }
        match (self.status.as_str(), self.payment_status.as_str()) {
            ("open", "unpaid") => {
                let raw = self
                    .url
                    .as_deref()
                    .filter(|v| v.len() <= 8192)
                    .ok_or(Error::Mismatch)?;
                let u = Url::parse(raw).map_err(|_| Error::Mismatch)?;
                if u.scheme() != "https"
                    || u.host_str() != Some("checkout.stripe.com")
                    || !u.username().is_empty()
                    || u.password().is_some()
                    || u.port().is_some()
                    || u.path() != format!("/c/pay/{}", self.id)
                {
                    return Err(Error::Mismatch);
                }
                Ok(("open", Some(raw)))
            }
            ("complete", "paid") => Ok(("paid_review", None)),
            ("expired", "unpaid") => Ok(("expired", None)),
            _ => Ok(("review_required", None)),
        }
    }
}

#[derive(Deserialize)]
struct Items {
    data: Vec<Item>,
    has_more: bool,
}
#[derive(Deserialize)]
struct Item {
    quantity: i64,
    currency: String,
    amount_total: i64,
    price: Price,
}
#[derive(Deserialize)]
struct Price {
    id: String,
    livemode: bool,
    unit_amount: i64,
    currency: String,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum IntentRef {
    Id(String),
    Expanded(Box<Intent>),
}
#[derive(Deserialize)]
struct Intent {
    id: String,
    object: String,
    livemode: bool,
    amount: i64,
    amount_received: i64,
    currency: String,
    status: String,
    metadata: Metadata,
    latest_charge: Charge,
}
#[derive(Deserialize)]
struct Charge {
    id: String,
    object: String,
    payment_intent: String,
    livemode: bool,
    paid: bool,
    captured: bool,
    refunded: bool,
    disputed: bool,
    amount: i64,
    amount_captured: i64,
    amount_refunded: i64,
    currency: String,
}
impl Session {
    pub(super) fn paid_evidence(&self, price: &str) -> Result<(&str, &str), Error> {
        let Some(items) = &self.line_items else {
            return Err(Error::Mismatch);
        };
        let [item] = &items.data[..] else {
            return Err(Error::Mismatch);
        };
        let intent = match &self.payment_intent {
            Some(IntentRef::Expanded(v)) => v,
            Some(IntentRef::Id(id)) if identifier(id, "pi_") => return Err(Error::Unavailable),
            _ => return Err(Error::Mismatch),
        };
        let ch = &intent.latest_charge;
        if self.status != "complete"
            || self.payment_status != "paid"
            || items.has_more
            || item.quantity != 1
            || item.amount_total != self.amount_total
            || item.currency != self.currency
            || item.price.id != price
            || item.price.livemode
            || item.price.unit_amount != self.amount_total
            || item.price.currency != self.currency
            || intent.object != "payment_intent"
            || !identifier(&intent.id, "pi_")
            || intent.livemode
            || intent.amount != self.amount_total
            || intent.amount_received != self.amount_total
            || intent.currency != self.currency
            || intent.status != "succeeded"
            || intent.metadata.orange_binding != self.metadata.orange_binding
            || intent.metadata.orange_order_id != self.metadata.orange_order_id
            || ch.object != "charge"
            || !identifier(&ch.id, "ch_")
            || ch.payment_intent != intent.id
            || ch.livemode
            || !ch.paid
            || !ch.captured
            || ch.refunded
            || ch.disputed
            || ch.amount_refunded != 0
            || ch.amount != self.amount_total
            || ch.amount_captured != self.amount_total
            || ch.currency != self.currency
        {
            return Err(Error::Mismatch);
        }
        Ok((&intent.id, &ch.id))
    }
}

#[derive(Deserialize)]
pub(super) struct RefundObject {
    pub id: String,
    object: String,
    charge: String,
    payment_intent: String,
    amount: i64,
    currency: String,
    status: Option<String>,
    livemode: Option<bool>,
    metadata: std::collections::BTreeMap<String, String>,
}
pub(super) struct RefundTerms {
    pub refund: String,
    pub order: String,
    pub currency: String,
    pub amount: i64,
    pub original_amount: i64,
    pub prior: i64,
    pub charge: String,
    pub intent: String,
    pub binding: String,
}
impl RefundObject {
    pub(super) fn matches(&self, t: &RefundTerms) -> bool {
        self.object == "refund"
            && identifier(&self.id, "re_")
            && self.livemode != Some(true)
            && self.charge == t.charge
            && self.payment_intent == t.intent
            && self.amount == t.amount
            && self.currency == t.currency
            && self.metadata.get("orange_refund_id") == Some(&t.refund)
            && self.metadata.get("orange_order_id") == Some(&t.order)
            && self.metadata.get("orange_binding") == Some(&t.binding)
    }
    pub(super) fn state(&self) -> &str {
        self.status.as_deref().unwrap_or("unknown")
    }
}
impl Sandbox {
    pub(super) async fn check_refundable_charge(&self, t: &RefundTerms) -> Result<(), Error> {
        self.account().await?;
        if !identifier(&t.charge, "ch_") {
            return Err(Error::Mismatch);
        }
        let v: Charge = receive(
            self.request(reqwest::Method::GET, &format!("charges/{}", t.charge))?
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await?;
        if v.object != "charge"
            || v.id != t.charge
            || v.payment_intent != t.intent
            || v.livemode
            || !v.paid
            || !v.captured
            || v.refunded
            || v.disputed
            || v.amount != t.original_amount
            || v.amount_captured != t.original_amount
            || v.amount_refunded != t.prior
            || v.currency != t.currency
        {
            return Err(Error::Mismatch);
        }
        Ok(())
    }
    pub(super) async fn create_refund(&self, t: &RefundTerms) -> Result<RefundObject, Error> {
        let amount = t.amount.to_string();
        let fields = [
            ("charge", t.charge.as_str()),
            ("amount", amount.as_str()),
            ("metadata[orange_refund_id]", &t.refund),
            ("metadata[orange_order_id]", &t.order),
            ("metadata[orange_binding]", &t.binding),
        ];
        receive(
            self.request(reqwest::Method::POST, "refunds")?
                .header("Idempotency-Key", format!("orange-refund-{}", t.refund))
                .form(&fields)
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await
    }
    pub(super) async fn refund(&self, id: &str) -> Result<RefundObject, Error> {
        if !identifier(id, "re_") {
            return Err(Error::Mismatch);
        }
        self.account().await?;
        let v: RefundObject = receive(
            self.request(reqwest::Method::GET, &format!("refunds/{id}"))?
                .send()
                .await
                .map_err(|_| Error::Unavailable)?,
        )
        .await?;
        if v.id != id {
            return Err(Error::Mismatch);
        }
        Ok(v)
    }
    pub(super) async fn find_refund(
        &self,
        t: &RefundTerms,
        after: Option<&str>,
    ) -> Result<(Option<RefundObject>, Option<String>), Error> {
        #[derive(Deserialize)]
        struct Page {
            object: String,
            data: Vec<RefundObject>,
            has_more: bool,
        }
        self.account().await?;
        let mut query = self
            .request(reqwest::Method::GET, "refunds")?
            .query(&[("charge", t.charge.as_str()), ("limit", "100")]);
        if let Some(id) = after {
            if !identifier(id, "re_") {
                return Err(Error::Invalid);
            }
            query = query.query(&[("starting_after", id)]);
        }
        let page: Page = receive(query.send().await.map_err(|_| Error::Unavailable)?).await?;
        if page.object != "list" || page.data.len() > 100 {
            return Err(Error::Mismatch);
        }
        let next = if page.has_more {
            Some(
                page.data
                    .last()
                    .filter(|v| identifier(&v.id, "re_") && Some(v.id.as_str()) != after)
                    .ok_or(Error::Mismatch)?
                    .id
                    .clone(),
            )
        } else {
            None
        };
        let mut candidates = page
            .data
            .into_iter()
            .filter(|r| r.metadata.get("orange_refund_id") == Some(&t.refund));
        let found = candidates.next();
        if candidates.next().is_some() {
            return Err(Error::Mismatch);
        }
        Ok((found, next))
    }
}
