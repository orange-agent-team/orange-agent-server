use super::*;
use std::sync::Arc;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source_id: String,
    owner_id: String,
    credential_revision: String,
    registration: Registration,
    credential: Credential,
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
                .map_err(|_| "invalid Confluence configuration")?;
        if entries.len() > 32 {
            return Err("too many Confluence sources");
        }
        let mut sources = BTreeMap::new();
        for c in entries {
            if [&c.source_id, &c.owner_id, &c.credential_revision]
                .iter()
                .any(|id| !orange_protocol::valid_id(id))
            {
                return Err("invalid source or credential revision");
            }
            let mode = match c.credential {
                Credential::CloudOauth { .. } => "cloud_oauth",
                Credential::CloudToken { .. } => "cloud_token",
                Credential::SiteToken { .. } => "site_token",
            };
            let digest = orange_protocol::request_digest(
                "confluence.registration.v1",
                &(
                    &c.registration,
                    &c.source_id,
                    &c.owner_id,
                    &c.credential_revision,
                    mode,
                ),
            )
            .map_err(|_| "invalid source digest")?;
            let reader = c
                .registration
                .open(&c.credential)
                .map_err(|_| "invalid Confluence source")?;
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
                return Err("duplicate Confluence source ID");
            }
        }
        Ok(Self(sources))
    }
}
