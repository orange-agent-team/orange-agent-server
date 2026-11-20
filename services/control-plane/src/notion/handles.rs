use super::*;
use crate::document_handles as nav;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    pub page: String,
    pub data_source: Option<String>,
    pub blocks: Vec<String>,
}
pub(super) struct Seals(nav::Seals);
fn map(e: nav::Error) -> Error {
    match e {
        nav::Error::Invalid => Error::Invalid,
        nav::Error::Limit => Error::Limit,
        nav::Error::Denied => Error::Denied,
    }
}
impl Seals {
    pub fn new(token: &str, registration: &Registration) -> Result<Self, Error> {
        nav::Seals::new(b"orange.notion.navigation.v1\0", token, registration)
            .map(Self)
            .map_err(map)
    }
    pub fn child(&self, s: &Scope, k: &str, v: &Location) -> Result<String, Error> {
        self.0.seal(&s.grant, "location", k, v).map_err(map)
    }
    pub fn location(&self, s: &Scope, k: &str, v: &str) -> Result<Location, Error> {
        self.0.open(&s.grant, "location", k, v).map_err(map)
    }
    pub fn next(&self, s: &Scope, k: &str, c: &str) -> Result<String, Error> {
        if c.is_empty() || c.len() > 1024 {
            return Err(Error::Protocol);
        }
        self.0.seal(&s.grant, "cursor", k, c).map_err(map)
    }
    pub fn cursor(&self, s: &Scope, k: &str, c: Option<&str>) -> Result<Option<String>, Error> {
        c.map(|v| self.0.open(&s.grant, "cursor", k, v))
            .transpose()
            .map_err(map)
    }
}
