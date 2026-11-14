//! Internal E03 support package, not another core service. File-descriptor-only
//! security metadata handling; no ambient paths, process launch or networking.
#![deny(unsafe_code)]
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    NativeCaptureWitness, SecurityMetadata, copy_publication_metadata, copy_security_metadata,
    security_metadata,
};
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
mod apple;
