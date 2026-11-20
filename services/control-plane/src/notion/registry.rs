use super::*;
use std::sync::Arc;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source_id: String,
    owner_id: String,
    credential_revision: String,
    token: String,
    registration: Registration,
}
pub struct Registered {
    pub owner: String,
    pub digest: String,
    pub registration: Registration,
    pub reader: Source,
}
#[derive(Default, Clone)]
pub struct Sources(pub BTreeMap<String, Arc<Registered>>);
impl Sources {
    #[cfg(unix)]
    pub fn load_private(path: &std::path::Path) -> Result<Self, &'static str> {
        let entries: Vec<Config> =
            serde_json::from_slice(&crate::connector_file::read_private(path)?)
                .map_err(|_| "invalid Notion configuration")?;
        if entries.len() > 32 {
            return Err("too many Notion sources");
        }
        let mut sources = BTreeMap::new();
        for c in entries {
            if !orange_protocol::valid_id(&c.source_id)
                || !orange_protocol::valid_id(&c.owner_id)
                || !orange_protocol::valid_id(&c.credential_revision)
            {
                return Err("invalid Notion source identity");
            }
            let reader = c
                .registration
                .open(&c.token)
                .map_err(|_| "invalid Notion registration")?;
            let normalized = reader.registration.clone();
            let digest = stamp(
                "notion.registration.v1",
                &(
                    &c.source_id,
                    &c.owner_id,
                    &c.credential_revision,
                    &normalized,
                    VERSION,
                ),
            )
            .map_err(|_| "invalid Notion scope")?;
            if sources
                .insert(
                    c.source_id,
                    Arc::new(Registered {
                        owner: c.owner_id,
                        digest,
                        registration: normalized,
                        reader,
                    }),
                )
                .is_some()
            {
                return Err("duplicate Notion source ID");
            }
        }
        Ok(Self(sources))
    }
}
