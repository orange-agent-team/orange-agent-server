//! Versioned per-registration MACs for navigation and continuation tokens. Grant liveness is checked separately.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::Sha256;
pub(crate) enum Error {
    Invalid,
    Limit,
    Denied,
}
pub(crate) struct Seals {
    key: Hmac<Sha256>,
}
impl Seals {
    pub fn new(
        namespace: &[u8],
        token: &str,
        registration: &impl Serialize,
    ) -> Result<Self, Error> {
        let mut derive =
            Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|_| Error::Invalid)?;
        derive.update(namespace);
        derive.update(&serde_json::to_vec(registration).map_err(|_| Error::Invalid)?);
        Ok(Self {
            key: Hmac::new_from_slice(&derive.finalize().into_bytes())
                .map_err(|_| Error::Invalid)?,
        })
    }
    pub fn seal<T: Serialize>(
        &self,
        grant: &str,
        domain: &str,
        binding: &str,
        data: T,
    ) -> Result<String, Error> {
        let bytes =
            serde_json::to_vec(&(domain, grant, binding, &data)).map_err(|_| Error::Invalid)?;
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
    pub fn open<T: DeserializeOwned>(
        &self,
        grant: &str,
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
        if d != domain || g != grant || b != binding {
            return Err(Error::Denied);
        }
        Ok(value)
    }
}
