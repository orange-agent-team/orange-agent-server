use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Owner,
    Admin,
    Member,
    Billing,
    Viewer,
}
impl TeamRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Billing => "billing",
            Self::Viewer => "viewer",
        }
    }
    pub fn can_create_content(self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Member)
    }
    pub fn can_read_content(self) -> bool {
        self != Self::Billing
    }
}
impl std::str::FromStr for TeamRole {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            "billing" => Ok(Self::Billing),
            "viewer" => Ok(Self::Viewer),
            _ => Err(()),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOrganization {
    pub name: String,
    pub member_limit: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationView {
    pub organization_id: String,
    pub name: String,
    pub member_limit: u16,
    pub policy_version: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeMembership {
    pub user_id: String,
    pub role: Option<TeamRole>,
    pub expected_policy_version: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipChange {
    pub organization_id: String,
    pub user_id: String,
    pub role: Option<TeamRole>,
    pub policy_version: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Read,
    Write,
}
impl ResourceAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeResourceAcl {
    pub user_id: String,
    pub access: Option<ResourceAccess>,
    pub expected_acl_version: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceView {
    pub resource_id: String,
    pub organization_id: String,
    pub creator_id: String,
    pub kind: String,
    pub acl_version: i64,
}
