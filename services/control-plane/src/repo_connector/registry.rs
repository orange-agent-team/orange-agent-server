use super::{Provider, Registration, Source};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source_id: String,
    owner_id: String,
    registration: Registration,
    /// A user-approved token is supplied out of band by the operator. Anonymous registrations omit it.
    token: Option<String>,
    credential_revision: Option<String>,
}
pub struct Registered {
    pub owner: String,
    pub digest: String,
    pub registration: Registration,
    pub reader: Source,
}
#[derive(Default, Clone)]
pub struct Sources(pub BTreeMap<String, Arc<Registered>>);
impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "github" => Some(Self::Github),
            "gitlab" => Some(Self::Gitlab),
            _ => None,
        }
    }
}
impl Sources {
    #[cfg(unix)]
    pub fn load_private(path: &std::path::Path) -> Result<Self, &'static str> {
        let entries: Vec<Config> =
            serde_json::from_slice(&crate::connector_file::read_private(path)?)
                .map_err(|_| "invalid repository source configuration")?;
        if entries.len() > 32 {
            return Err("too many repository sources");
        }
        let mut sources = BTreeMap::new();
        for c in entries {
            if !orange_protocol::valid_id(&c.source_id)
                || !orange_protocol::valid_id(&c.owner_id)
                || c.token.is_some() && c.credential_revision.is_none()
            {
                return Err("invalid owner or credential revision");
            }
            let digest = orange_protocol::request_digest(
                "repository.source.v1",
                &(
                    &c.source_id,
                    &c.owner_id,
                    &c.registration,
                    c.token.is_some(),
                ),
            )
            .map_err(|_| "invalid source")?;
            let digest =
                crate::connector_file::revision_digest(digest, c.credential_revision.as_deref())?;
            let reader = c
                .registration
                .open(c.token.as_deref())
                .map_err(|_| "invalid repository registration")?;
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
                return Err("duplicate source");
            }
        }
        Ok(Self(sources))
    }
}
