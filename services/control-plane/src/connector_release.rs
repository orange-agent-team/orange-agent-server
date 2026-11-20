//! An existing grant may only use the pinned semantics of an action in its original release.
use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Deserialize)]
struct Catalog {
    adapters: Vec<Adapter>,
}
#[derive(Deserialize)]
struct Adapter {
    provider: String,
    actions: Vec<Action>,
}
#[derive(Deserialize)]
struct Action {
    name: String,
    execution_digest: String,
}
type Pins = BTreeMap<String, BTreeMap<String, String>>;
pub(crate) fn pin(provider: &str, action: &str) -> Option<&'static str> {
    static PINS: OnceLock<Option<Pins>> = OnceLock::new();
    PINS.get_or_init(|| {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../../../config/connector-releases.v1.json"))
                .ok()?;
        let mut result = Pins::new();
        for adapter in catalog.adapters {
            let mut actions = BTreeMap::new();
            for action in adapter.actions {
                if !orange_protocol::valid_id(&action.name)
                    || !orange_protocol::valid_digest(&action.execution_digest)
                    || actions
                        .insert(action.name, action.execution_digest)
                        .is_some()
                {
                    return None;
                }
            }
            if result.insert(adapter.provider, actions).is_some() {
                return None;
            }
        }
        Some(result)
    })
    .as_ref()?
    .get(provider)?
    .get(action)
    .map(String::as_str)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_unknown_or_cross_provider_action_has_no_execution_pin() {
        assert!(pin("postgres", "query_view").is_some());
        assert_ne!(pin("github", "read_file"), pin("gitlab", "read_file"));
        assert!(pin("confluence", "query_database").is_none());
        assert!(pin("github", "comment").is_none());
        assert!(pin("unknown", "list_objects").is_none());
    }
}
