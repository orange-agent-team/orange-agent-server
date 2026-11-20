//! Signed navigation handles never grant authority by themselves: each use rechecks its live root.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use sha2::Sha256;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    pub page: String,
    pub data_source: Option<String>,
    pub blocks: Vec<String>,
}
pub(super) struct Seals {
    key: Hmac<Sha256>,
}
impl Seals {
    pub fn new(token: &str, registration: &Registration) -> Result<Self, Error> {
        let mut derive =
            Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|_| Error::Invalid)?;
        derive.update(b"orange.notion.navigation.v1\0");
        derive.update(&serde_json::to_vec(registration).map_err(|_| Error::Invalid)?);
        Ok(Self {
            key: Hmac::new_from_slice(&derive.finalize().into_bytes())
                .map_err(|_| Error::Invalid)?,
        })
    }
    fn seal<T: Serialize>(
        &self,
        scope: &Scope,
        domain: &str,
        binding: &str,
        data: T,
    ) -> Result<String, Error> {
        let bytes = serde_json::to_vec(&(domain, &scope.grant, binding, &data))
            .map_err(|_| Error::Invalid)?;
        if bytes.len() > 4096 {
            return Err(Error::Limit);
        }
        let mut mac = self.key.clone();
        mac.update(&bytes);
        Ok(format!(
            "{}.{}",
            B64.encode(&bytes),
            B64.encode(mac.finalize().into_bytes())
        ))
    }
    fn open<T: DeserializeOwned>(
        &self,
        scope: &Scope,
        domain: &str,
        binding: &str,
        s: &str,
    ) -> Result<T, Error> {
        if s.len() > 6144 {
            return Err(Error::Denied);
        }
        let (doc, tag) = s.split_once('.').ok_or(Error::Denied)?;
        let doc = B64.decode(doc).map_err(|_| Error::Denied)?;
        let tag = B64.decode(tag).map_err(|_| Error::Denied)?;
        let mut mac = self.key.clone();
        mac.update(&doc);
        mac.verify_slice(&tag).map_err(|_| Error::Denied)?;
        let (d, g, b, value): (String, String, String, T) =
            serde_json::from_slice(&doc).map_err(|_| Error::Denied)?;
        if d != domain || g != scope.grant || b != binding {
            return Err(Error::Denied);
        }
        Ok(value)
    }
    pub fn child(&self, s: &Scope, k: &str, v: &Location) -> Result<String, Error> {
        self.seal(s, "location", k, v)
    }
    pub fn location(&self, s: &Scope, k: &str, v: &str) -> Result<Location, Error> {
        self.open(s, "location", k, v)
    }
    pub fn next(&self, s: &Scope, k: &str, c: &str) -> Result<String, Error> {
        if c.is_empty() || c.len() > 1024 {
            return Err(Error::Protocol);
        }
        self.seal(s, "cursor", k, c)
    }
    pub fn cursor(&self, s: &Scope, k: &str, c: Option<&str>) -> Result<Option<String>, Error> {
        c.map(|v| self.open(s, "cursor", k, v)).transpose()
    }
}
