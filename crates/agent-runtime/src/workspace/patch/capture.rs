//! Durable evidence for the narrow OS-added quarantine transition. No generic
//! "ignore security" comparison, label rewriting or filesystem authority.
use super::{PatchError, authority::SourceIdentity};
use crate::{Journal, JournalError};
use orange_platform_files::{NativeCaptureWitness, SecurityMetadata};
use orange_protocol::{request_digest, valid_digest};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureWitness {
    operation_id: String,
    source_security_digest: String,
    native_authority_digest: String,
    not_before: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CaptureEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) witness: Option<CaptureWitness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retained_security_digest: Option<String>,
}
impl CaptureWitness {
    pub(crate) fn observe(
        operation: &str,
        source: &SecurityMetadata,
        staged: &SecurityMetadata,
    ) -> Option<Self> {
        NativeCaptureWitness::observe(source, staged).map(|w| Self {
            operation_id: operation.into(),
            source_security_digest: hex::encode(w.source_digest),
            native_authority_digest: hex::encode(w.authority_digest),
            not_before: w.not_before,
        })
    }
    pub(crate) fn digest(&self) -> Result<String, PatchError> {
        request_digest("patch-native-capture-witness.v1", self).map_err(|_| PatchError::Invalid)
    }
    fn native(&self) -> Result<NativeCaptureWitness, PatchError> {
        fn digest(value: &str) -> Result<[u8; 32], PatchError> {
            if !valid_digest(value) {
                return Err(PatchError::Invalid);
            }
            hex::decode(value)
                .map_err(|_| PatchError::Invalid)?
                .try_into()
                .map_err(|_| PatchError::Invalid)
        }
        Ok(NativeCaptureWitness {
            source_digest: digest(&self.source_security_digest)?,
            authority_digest: digest(&self.native_authority_digest)?,
            not_before: self.not_before,
        })
    }
}
impl CaptureEvidence {
    pub(super) fn source_bound(&self, source: &SourceIdentity) -> bool {
        self.witness
            .as_ref()
            .is_none_or(|w| w.source_security_digest == source.security_digest())
            && self
                .retained_security_digest
                .as_ref()
                .is_none_or(|d| valid_digest(d))
    }
    pub(super) fn accepts_security(
        &self,
        source: &SourceIdentity,
        current: &SecurityMetadata,
        retained: bool,
    ) -> bool {
        if !self.source_bound(source) {
            return false;
        }
        let digest = hex::encode(current.digest());
        if self
            .retained_security_digest
            .as_ref()
            .is_some_and(|expected| &digest != expected)
        {
            return false;
        }
        if !retained
            && self.retained_security_digest.is_none()
            && digest == source.security_digest()
        {
            return true;
        }
        match &self.witness {
            Some(w) => w.native().is_ok_and(|w| w.verifies(current)),
            None => digest == source.security_digest(),
        }
    }
    pub(super) fn accepts_identity(
        &self,
        source: &SourceIdentity,
        current: &SourceIdentity,
        security: &SecurityMetadata,
    ) -> bool {
        if current.security_digest() != hex::encode(security.digest())
            || !self.accepts_security(source, security, true)
        {
            return false;
        }
        let mut expected = source.clone();
        expected.bind_security(security.digest());
        expected.matches_after_rename(current)
    }
}
impl Journal {
    pub(crate) fn capture_evidence(
        &self,
        owner: &str,
        operation: &str,
    ) -> Result<CaptureEvidence, PatchError> {
        let owned:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id) WHERE o.operation_id=? AND o.owner=?)",params![operation,owner],|r|r.get(0)).map_err(JournalError::from)?;
        if !owned {
            return Err(PatchError::Invalid);
        }
        let raw: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT body,digest FROM patch_capture_witnesses WHERE operation_id=?",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(JournalError::from)?;
        let witness = if let Some((body, digest)) = raw {
            if body.len() > 4096 {
                return Err(PatchError::Limit);
            }
            let value: CaptureWitness =
                serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if value.operation_id != operation || value.digest()? != digest {
                return Err(PatchError::Invalid);
            }
            value.native()?;
            let present:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM editing_publication_steps WHERE operation_id=? AND step=2)",[operation],|r|r.get(0)).map_err(JournalError::from)?;
            if !present {
                return Err(PatchError::Invalid);
            }
            Some(value)
        } else {
            None
        };
        let retained_security_digest: Option<String> = self
            .connection
            .query_row(
                "SELECT security_digest FROM patch_capture_receipts WHERE operation_id=?",
                [operation],
                |r| r.get(0),
            )
            .optional()
            .map_err(JournalError::from)?;
        if let Some(digest) = &retained_security_digest {
            let present:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM editing_publication_steps WHERE operation_id=? AND step=4)",[operation],|r|r.get(0)).map_err(JournalError::from)?;
            if !present || !valid_digest(digest) {
                return Err(PatchError::Invalid);
            }
        }
        Ok(CaptureEvidence {
            witness,
            retained_security_digest,
        })
    }
}
