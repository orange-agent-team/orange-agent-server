//! Immutable object descriptors. Decimal lengths never pass through JS floats.
use crate::{valid_digest, valid_id};
use serde::{Deserialize, Serialize};
pub const OBJECT_LIMIT: u64 = 100 * 1024 * 1024;
pub const MEDIA_LIMIT: u64 = 100_000_000;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateUpload {
    pub upload_id: String,
    pub sha256: String,
    pub size_bytes: String,
    pub media_type: String,
}
impl CreateUpload {
    pub fn size(&self) -> Option<u64> {
        if !valid_id(&self.upload_id)
            || !self
                .upload_id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !valid_digest(&self.sha256)
            || self.size_bytes.is_empty()
            || self.size_bytes.len() > 1 && self.size_bytes.starts_with('0')
            || !self.size_bytes.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let size = self.size_bytes.parse::<u64>().ok()?;
        let media = matches!(
            self.media_type.as_str(),
            "image/png"
                | "image/jpeg"
                | "image/webp"
                | "audio/wav"
                | "audio/mpeg"
                | "audio/flac"
                | "audio/ogg"
                | "video/mp4"
        );
        if !media
            && !matches!(
                self.media_type.as_str(),
                "application/octet-stream" | "application/json" | "text/plain"
            )
        {
            return None;
        }
        (size <= if media { MEDIA_LIMIT } else { OBJECT_LIMIT }).then_some(size)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactView {
    pub upload_id: String,
    pub artifact_id: String,
    pub version_id: String,
    pub owner_id: String,
    pub sha256: String,
    pub size_bytes: String,
    pub media_type: String,
    pub state: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lengths_are_canonical_and_media_has_the_stricter_limit() {
        let mut r = CreateUpload {
            upload_id: "upload".into(),
            sha256: "a".repeat(64),
            size_bytes: OBJECT_LIMIT.to_string(),
            media_type: "application/octet-stream".into(),
        };
        assert_eq!(r.size(), Some(OBJECT_LIMIT));
        r.media_type = "audio/wav".into();
        assert!(r.size().is_none());
        r.size_bytes = MEDIA_LIMIT.to_string();
        assert_eq!(r.size(), Some(MEDIA_LIMIT));
        for s in ["01", "-1", "1e9", "18446744073709551616"] {
            r.size_bytes = s.into();
            assert!(r.size().is_none());
        }
        assert!(
            serde_json::from_str::<CreateUpload>(
                r#"{"upload_id":"u","sha256":"a","size_bytes":1,"media_type":"text/plain"}"#
            )
            .is_err()
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindJobInput {
    pub upload_id: String,
    pub version_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobInputView {
    pub job_id: String,
    pub artifact_id: String,
    pub version_id: String,
    pub sha256: String,
    pub size_bytes: String,
    pub projection_digest: String,
}
