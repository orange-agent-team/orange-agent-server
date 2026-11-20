//! Final, versioned transcription document. Input measurement is checked against the issuer's immutable grant.
use crate::pricing::AudioMeasurement;
use serde::{Deserialize, Serialize};

pub const MEDIA: &str = "application/vnd.orange.transcription+json";
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimedText {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionResult {
    pub version: u32,
    pub request_projection_digest: String,
    pub execution_profile_digest: String,
    pub measurement: AudioMeasurement,
    pub language: String,
    pub language_probability: f64,
    pub compute_type: String,
    pub segments: Vec<TimedText>,
    pub text: String,
}
impl TranscriptionResult {
    pub fn matches(
        &self,
        measured: &AudioMeasurement,
        projection: &str,
        profile: &str,
        compute: &str,
        language: Option<&str>,
    ) -> bool {
        if self.version != 1
            || self.measurement != *measured
            || measured.model_version != "whisper-small-ct2-int8-cpu-v1"
            || self.request_projection_digest != projection
            || self.execution_profile_digest != profile
            || !crate::valid_digest(projection)
            || !crate::valid_digest(profile)
            || self.compute_type != compute
            || !matches!(compute, "int8" | "int8_float32")
            || !(0.0..=1.0).contains(&self.language_probability)
            || language.is_some_and(|code| code != self.language)
            || self.segments.is_empty()
            || self.segments.len() > 10000
            || self.text.trim().is_empty()
            || self.text.len() > 1024 * 1024
        {
            return false;
        }
        let Ok(duration) = measured.duration_ms.parse::<u64>() else {
            return false;
        };
        if !(1..=600000).contains(&duration) || duration.to_string() != measured.duration_ms {
            return false;
        }
        let Ok(pin) = serde_json::from_slice::<serde_json::Value>(include_bytes!(
            "../../../config/local-transcription.lock.json"
        )) else {
            return false;
        };
        if !pin["language_codes"]
            .as_array()
            .is_some_and(|codes| codes.iter().any(|code| code == &self.language))
        {
            return false;
        }
        let mut text = String::new();
        let mut last = 0;
        for seg in &self.segments {
            if seg.start_ms < last
                || seg.end_ms < seg.start_ms
                || seg.end_ms > duration
                || text.len() + seg.text.len() > 1024 * 1024
            {
                return false;
            }
            last = seg.end_ms;
            text.push_str(&seg.text);
        }
        text.trim() == self.text
    }
}
