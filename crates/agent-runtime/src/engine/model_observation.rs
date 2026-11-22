use super::*;

/// Metadata about an unconfirmed dispatch. An observation is never permission to run it again.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelObservation {
    pub invocation: String,
    pub kind: String,
    pub observed_at_unix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_limit_ms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<String>,
}

pub(super) struct DispatchObservation<'a> {
    pub invocation: &'a str,
    pub kind: &'static str,
    pub wait_limit: Duration,
    pub started: tokio::time::Instant,
}

impl Journal {
    pub fn latest_model_observation(
        &self,
        owner: &str,
        run: &str,
    ) -> Result<Option<ModelObservation>, EngineError> {
        let observation=self.connection.query_row(
            "SELECT o.invocation,o.kind,o.observed_at,o.wait_limit_ms,o.elapsed_ms
             FROM runs r LEFT JOIN runtime_model_observations o ON o.invocation=(
                 SELECT i.invocation FROM runtime_invocations i JOIN runtime_model_observations m USING(invocation)
                 WHERE i.run_id=r.run_id AND i.state='unknown' ORDER BY i.ordinal DESC LIMIT 1)
             WHERE r.run_id=? AND r.owner=? AND r.active",
            params![run,owner], |r|{
                let Some(invocation)=r.get::<_,Option<String>>(0)? else {return Ok(None);};
                Ok(Some(ModelObservation{
                    invocation,kind:r.get(1)?,observed_at_unix:r.get::<_,i64>(2)?.to_string(),
                    wait_limit_ms:r.get::<_,Option<i64>>(3)?.map(|n|n.to_string()),elapsed_ms:r.get::<_,Option<i64>>(4)?.map(|n|n.to_string())
                }))
            }
        ).optional()?;
        observation.ok_or(EngineError::Forbidden)
    }
}
