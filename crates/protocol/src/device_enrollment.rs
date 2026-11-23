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
