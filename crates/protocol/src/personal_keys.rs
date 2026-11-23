//! Personal account automation. The client retains the bearer and registers its verifier.
use serde::{Deserialize, Serialize};

pub const AUDIENCE: &str = "orange-personal-automation-v1";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ReadScope {
    #[serde(rename = "wallet:read")]
    Wallet,
    #[serde(rename = "budgets:read")]
    Budgets,
}
impl ReadScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet:read",
            Self::Budgets => "budgets:read",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterPersonalKey {
    pub label: String,
    pub audience: String,
    pub scope: ReadScope,
    pub verifier: String,
    pub ttl_seconds: i32,
}

impl RegisterPersonalKey {
    pub fn valid(&self) -> bool {
        self.audience == AUDIENCE
            && !self.label.trim().is_empty()
            && self.label == self.label.trim()
            && self.label.chars().count() <= 64
            && !self.label.chars().any(char::is_control)
            && (60..=2_592_000).contains(&self.ttl_seconds)
            && crate::valid_digest(&self.verifier)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalKeyView {
    pub key_id: String,
    pub user_id: String,
    pub owner_id: String,
    pub audience: String,
    pub scope: ReadScope,
    pub label: String,
    pub status: KeyStatus,
    pub created_at: String,
    pub expires_at: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    Active,
    Expired,
    Revoked,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalKeyPage {
    pub data: Vec<PersonalKeyView>,
    pub next_cursor: Option<String>,
}
