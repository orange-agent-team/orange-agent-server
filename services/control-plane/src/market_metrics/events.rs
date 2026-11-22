//! Closed, pseudonymous report input. No catch-all fields for free-form product content.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Key(String);
impl TryFrom<String> for Key {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if orange_protocol::valid_digest(&s) {
            Ok(Self(s))
        } else {
            Err("expected a lowercase 256-bit pseudonymous reference")
        }
    }
}
impl From<Key> for String {
    fn from(k: Key) -> Self {
        k.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Units(pub i64);
impl TryFrom<String> for Units {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let n = s
            .parse::<i64>()
            .map_err(|_| "expected nonnegative canonical integer")?;
        if n < 0 || n.to_string() != s {
            return Err("expected nonnegative canonical integer");
        }
        Ok(Self(n))
    }
}
impl From<Units> for String {
    fn from(n: Units) -> String {
        n.0.to_string()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Currency(String);
impl TryFrom<String> for Currency {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 3 && s.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(s))
        } else {
            Err("expected lowercase currency code")
        }
    }
}
impl From<Currency> for String {
    fn from(c: Currency) -> String {
        c.0
    }
}
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostKind {
    ProviderFulfillment,
    PlatformCompute,
    RetrySubsidy,
    Storage,
    Egress,
    PaymentProcessing,
}
pub const COST_KINDS: [CostKind; 6] = [
    CostKind::ProviderFulfillment,
    CostKind::PlatformCompute,
    CostKind::RetrySubsidy,
    CostKind::Storage,
    CostKind::Egress,
    CostKind::PaymentProcessing,
];
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LotUse {
    pub order: Key,
    pub units: Units,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityState {
    Eligible,
    Paused,
    Disconnected,
    Quarantined,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Delivered,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Purchase {
    pub order: Key,
    pub payer: Key,
    pub currency: Currency,
    pub minor: Units,
    pub units: Units,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settlement {
    pub job: Key,
    pub charge: Units,
    pub gift: Units,
    pub earned: Units,
    pub purchased: Units,
    pub lots: Vec<LotUse>,
    pub provider_pending: Units,
    pub platform_fee: Units,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fact {
    NodeRegistered {
        node: Key,
    },
    Capacity {
        node: Key,
        slot: Key,
        until: u64,
        state: CapacityState,
    },
    JobSubmitted {
        job: Key,
        payer: Key,
    },
    Attempt {
        attempt: Key,
        job: Key,
        node: Key,
        slot: Key,
        until: u64,
    },
    JobFinished {
        job: Key,
        outcome: Outcome,
        winner: Option<Key>,
    },
    Purchase(Purchase),
    Refund {
        refund: Key,
        order: Key,
        minor: Units,
        units: Units,
    },
    Chargeback {
        dispute: Key,
        order: Key,
        minor: Units,
    },
    ChargebackRecovery {
        dispute: Key,
        minor: Units,
    },
    Settlement(Settlement),
    EarningReleased {
        job: Key,
        units: Units,
    },
    Expense {
        currency: Currency,
        category: CostKind,
        minor: Units,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub receipt: Key,
    pub at: u64,
    pub fact: Fact,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub receipt: Key,
    pub currency: Currency,
    pub complete: Vec<CostKind>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub population: Population,
    pub version: u8,
    pub supply_history_complete: bool,
    pub earning_releases_complete: bool,
    pub from_unix: u64,
    pub until_unix: u64,
    pub as_of_unix: u64,
    pub events: Vec<Event>,
    pub cost_coverage: Vec<Coverage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Population {
    ActivityExtract,
    SettledPayerCohort,
}
