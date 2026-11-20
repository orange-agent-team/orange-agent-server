//! Operator configuration for local development data sources.
use crate::pg_views::Registration;
use serde::Deserialize;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Duration};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    source_id: String,
    owner_id: String,
    address: IpAddr,
    port: u16,
    password: String,
    registration: Registration,
}
pub struct Source {
    pub owner_id: String,
    pub digest: String,
    pub registration: Registration,
    pub pool: PgPool,
}
#[derive(Clone, Default)]
pub struct Sources(pub BTreeMap<String, Arc<Source>>);
impl Sources {
    #[cfg(unix)]
    pub fn load_development_file(path: &std::path::Path) -> Result<Self, &'static str> {
        let bytes = crate::connector_file::read_private(path)?;
        Self::parse(&bytes)
    }
    fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let entries: Vec<Config> =
            serde_json::from_slice(bytes).map_err(|_| "invalid source configuration")?;
        if entries.len() > 32 {
            return Err("too many sources");
        }
        let mut result = BTreeMap::new();
        for c in entries {
            if !orange_protocol::valid_id(&c.source_id)
                || !orange_protocol::valid_id(&c.owner_id)
                || !c.address.is_loopback()
                || c.port == 0
                || c.password.is_empty()
                || c.registration.validate().is_err()
            {
                return Err("invalid development source");
            }
            let digest = orange_protocol::request_digest(
                "cn05.registration.v1",
                &(
                    &c.source_id,
                    &c.owner_id,
                    c.address,
                    c.port,
                    &c.registration,
                ),
            )
            .map_err(|_| "invalid source identity")?;
            let options = PgConnectOptions::new()
                .host(&c.address.to_string())
                .port(c.port)
                .database(&c.registration.database)
                .username(&c.registration.read_role)
                .password(&c.password)
                .ssl_mode(PgSslMode::Disable);
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(3))
                .connect_lazy_with(options);
            if result
                .insert(
                    c.source_id,
                    Arc::new(Source {
                        owner_id: c.owner_id,
                        digest,
                        registration: c.registration,
                        pool,
                    }),
                )
                .is_some()
            {
                return Err("duplicate source identifier");
            }
        }
        Ok(Self(result))
    }
}
