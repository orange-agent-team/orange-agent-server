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
