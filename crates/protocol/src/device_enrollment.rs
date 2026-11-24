//! Public PKCS#10 request, created on-device before any certificate or execution authority exists.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCertificateRequest {
    pub version: u32,
    pub request_id: String,
    pub purpose: String,
    pub spki_sha256: String,
    pub csr_sha256: String,
    pub csr_pem: String,
}

/// A logged-in node manager authorizes exactly this CSR and the hash of one random exchange token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeWorkerEnrollment {
    pub request: WorkerCertificateRequest,
    pub exchange_token_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerEnrollmentView {
    pub request_id: String,
    pub owner_id: String,
    pub requested_by: String,
    pub spki_sha256: String,
    pub csr_sha256: String,
    pub state: String,
    pub expires_unix: String,
}

/// A retry supplies the original redemption UUID and still requires the original enrollment token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemWorkerCertificate {
    pub version: u32,
    pub redemption_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCertificateReceipt {
    pub version: u32,
    pub request_id: String,
    pub redemption_id: String,
    pub node_id: String,
    pub spki_sha256: String,
    pub certificate_sha256: String,
    pub issuer_sha256: String,
    pub expires_unix: String,
    pub certificate_pem: String,
}

/// Signed by the installed device leaf. The replacement CSR independently proves the new key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRotationIntent {
    pub version: u32,
    pub owner_id: String,
    pub node_id: String,
    pub previous_request_id: String,
    pub previous_certificate_sha256: String,
    pub previous_issuer_sha256: String,
    pub replacement: AuthorizeWorkerEnrollment,
}
impl WorkerRotationIntent {
    /// Sign the lowercase ASCII digest using the old Ed25519 key. The domain is distinct from login and enrollment.
    pub fn signing_message(&self) -> Result<String, serde_json::Error> {
        crate::request_digest("worker-certificate-rotation.v1", self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRotationHandoff {
    pub intent: WorkerRotationIntent,
    pub previous_key_signature: String,
}
