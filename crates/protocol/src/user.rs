use crate::team::TeamRole;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSessionView {
    pub session_id: String,
    pub user_id: String,
    pub personal_owner_id: String,
    pub csrf_token: String,
    pub expires_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserOrganization {
    pub member_user_id: String,
    pub organization_id: String,
    pub name: String,
    pub role: TeamRole,
    pub member_limit: u16,
    pub policy_version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserOrganizationList {
    pub user_id: String,
    pub data: Vec<UserOrganization>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRevocationContext {
    pub csrf_token: String,
}
