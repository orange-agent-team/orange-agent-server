//! Stable local connection identity and model facts shared by trusted native hosts.
use crate::{model::ModelRequestError, request_digest, valid_digest, valid_id};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct LocalWeightPin {
    pub sha256: String,
    pub bytes: u64,
    pub context_tokens: u32,
}
pub fn local_7b_pin() -> Result<LocalWeightPin, ModelRequestError> {
    let pin: LocalWeightPin = serde_json::from_str(include_str!(
        "../../../config/local-code-review-model.lock.json"
    ))
    .map_err(|_| ModelRequestError::Invalid)?;
    if !valid_digest(&pin.sha256) || pin.bytes == 0 || !(256..=32768).contains(&pin.context_tokens)
    {
        return Err(ModelRequestError::Invalid);
    }
    Ok(pin)
}

#[derive(Clone, Debug)]
pub struct LocalModelIdentity {
    pub alias: String,
    pub context_tokens: u32,
    pub model_sha256: String,
    pub executable_sha256: String,
}
impl LocalModelIdentity {
    /// The desktop already uses this key in saved Run configurations. Keep its tuple and domain stable.
    pub fn connection_id(&self) -> Result<String, ModelRequestError> {
        if !valid_id(&self.alias)
            || !(256..=32768).contains(&self.context_tokens)
            || !valid_digest(&self.model_sha256)
            || !valid_digest(&self.executable_sha256)
        {
            return Err(ModelRequestError::Invalid);
        }
        let digest = request_digest(
            "desktop-model-v1",
            &(
                &self.alias,
                self.context_tokens,
                &self.model_sha256,
                &self.executable_sha256,
            ),
        )
        .map_err(|_| ModelRequestError::Invalid)?;
        Ok(format!("local-{digest}"))
    }

    pub fn matches_local_7b(&self) -> Result<bool, ModelRequestError> {
        let pin = local_7b_pin()?;
        Ok(self.model_sha256 == pin.sha256 && self.context_tokens == pin.context_tokens)
    }
}
