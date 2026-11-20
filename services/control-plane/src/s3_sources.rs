//! The runtime registry owns the only credentials for the external source.
use crate::s3_objects::{Registration, Source};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source_id: String,
    owner_id: String,
    registration: Registration,
    access_key: String,
    secret_key: String,
}
pub struct Registered {
    pub owner: String,
    pub digest: String,
    pub registration: Registration,
    pub reader: Source,
}
#[derive(Clone, Default)]
pub struct Sources(pub BTreeMap<String, Arc<Registered>>);
impl Sources {
    #[cfg(unix)]
    pub fn load_development_file(path: &std::path::Path) -> Result<Self, &'static str> {
        let bytes = crate::connector_file::read_private(path)?;
        let entries: Vec<Config> =
            serde_json::from_slice(&bytes).map_err(|_| "invalid S3 source config")?;
        if entries.len() > 32 {
            return Err("too many S3 sources");
        }
        let mut sources = BTreeMap::new();
        for c in entries {
            if !orange_protocol::valid_id(&c.owner_id) || !orange_protocol::valid_id(&c.source_id) {
                return Err("invalid source or owner");
            }
            let digest = orange_protocol::request_digest(
                "cn06.registration.v1",
                &(&c.source_id, &c.owner_id, &c.registration, &c.access_key),
            )
            .map_err(|_| "invalid registration")?;
            let reader = c
                .registration
                .open_development(&c.access_key, &c.secret_key)
                .map_err(|_| "invalid S3 source")?;
            if sources
                .insert(
                    c.source_id,
                    Arc::new(Registered {
                        owner: c.owner_id,
                        digest,
                        registration: c.registration,
                        reader,
                    }),
                )
                .is_some()
            {
                return Err("duplicate S3 source");
            }
        }
        Ok(Self(sources))
    }
}
