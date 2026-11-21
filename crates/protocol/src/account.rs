//! Personal account metadata. Balances are facts, not payment or spend authority.
use crate::{BudgetView, Money};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletView {
    pub user_id: String,
    pub owner_id: String,
    pub gift: Money,
    pub earned: Money,
    pub purchased: Money,
    pub reserved_gift: Money,
    pub reserved_earned: Money,
    pub reserved_purchased: Money,
    pub pending_earned: Money,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalBudgetList {
    pub user_id: String,
    pub owner_id: String,
    pub data: Vec<BudgetView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderIncomeState {
    Pending,
    Review,
    Available,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIncome {
    pub job_id: String,
    pub units: Money,
    pub eligible_unix: String,
    pub state: ProviderIncomeState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIncomeList {
    pub user_id: String,
    pub owner_id: String,
    pub data: Vec<ProviderIncome>,
    pub next_cursor: Option<String>,
}
