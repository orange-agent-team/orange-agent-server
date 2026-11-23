use super::{ControlConnection, ControlError, status};
use orange_protocol::{
    grpc::worker::{IsolationObservation, IsolationObservationReceipt},
    isolation,
};
use std::time::{Duration, Instant};

impl ControlConnection {
    /// Send only the explicitly selected diagnostic. The current connection owns
    /// the identity fields; no caller-selected node or destination is accepted.
    pub async fn report_isolation_observation(
        &mut self,
        request_id: &str,
        policy: &str,
        probe: &str,
        reported_capture_unix: i64,
        bytes: Vec<u8>,
    ) -> Result<IsolationObservationReceipt, ControlError> {
        if !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        let request = IsolationObservation {
            report_version: 1,
            session_id: self.identity.session_id.clone(),
            connection_epoch: self.identity.connection_epoch,
            node_policy_generation: Some(self.observed_policy_generation),
            request_id: request_id.into(),
            profile_id: isolation::PROFILE.into(),
            oci_policy_sha256: policy.into(),
            linux_probe_sha256: probe.into(),
            probe_json: bytes,
            reported_capture_unix,
        };
        let expected = isolation::check(&request).map_err(|_| ControlError::Invalid)?;
        let end = self.stop_at.min(Instant::now() + Duration::from_secs(5));
        let receipt = tokio::time::timeout_at(
            tokio::time::Instant::from_std(end),
            self.input_client.report_isolation_observation(request),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        if !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        if receipt.report_version != 1
            || receipt.session_id != self.identity.session_id
            || receipt.connection_epoch != self.identity.connection_epoch
            || receipt.request_id != request_id
            || receipt.observation_digest != expected.digest
            || receipt.disposition != expected.disposition
            || receipt.supply_authorized
            || receipt.wasm_enabled
            || uuid::Uuid::parse_str(&receipt.observation_id).is_err()
        {
            return Err(ControlError::Protocol);
        }
        Ok(receipt)
    }
}
