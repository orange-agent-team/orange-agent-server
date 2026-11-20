//! Fixed MC-05 CPU profile. Decoded usage and complete segments are candidates for host-side publication.
mod owned;
mod qualification;
pub use owned::{OwnedTranscription, TranscriptionAttempt, TranscriptionExit};
pub use qualification::{ExecutionProfile, ObservedRuntime, Qualification};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::watch,
};

pub const MODEL: &str = "whisper-small-ct2-int8-cpu-v1";
pub const MAX_INPUT: usize = 100_000_000;
const RUNNER: &str = include_str!("../../../backends/orange_inference_backends/transcription.py");
const PIN: &str = include_str!("../../../config/local-transcription.lock.json");
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid audio or transcription configuration")]
    Invalid,
    #[error("input or transcript exceeds the profile limit")]
    Limit,
    #[error("decoded input differs from the accepted measurement")]
    Changed,
    #[error("no speech detected")]
    NoSpeech,
    #[error("CPU transcription slot occupied")]
    Busy,
    #[error("transcription cancelled")]
    Cancelled,
    #[error("transcription deadline exceeded")]
    Timeout,
    #[error("invalid or incomplete runner output")]
    Protocol,
    #[error("transcription backend unavailable")]
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    pub preprocessing: String,
    pub format: String,
    pub codec: String,
    pub sample_rate: u32,
    pub source_samples: u64,
    pub normalized_samples: u64,
    pub normalized_sha256: String,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    pub compute_type: String,
    pub language: String,
    pub language_probability: f64,
    pub segments: Vec<Segment>,
    pub text: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub model_version: String,
    pub audio_sha256: String,
    pub measure: Measure,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Transcript>,
}
fn language_ok(s: &str) -> bool {
    (2..=3).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_lowercase())
}
impl Measure {
    pub fn validate(&self) -> Result<(), Error> {
        let rate = u64::from(self.sample_rate);
        if self.preprocessing != "mono-16khz-s16-f32-v1"
            || !(8000..=192000).contains(&rate)
            || !(1..=600 * rate).contains(&self.source_samples)
            || !(1..=9_600_000).contains(&self.normalized_samples)
            || !orange_protocol::valid_digest(&self.normalized_sha256)
            || self.duration_ms != (self.source_samples * 1000).div_ceil(rate)
            || (self.normalized_samples * rate).abs_diff(self.source_samples * 16000) > 2 * rate
        {
            return Err(Error::Protocol);
        }
        let supported = match self.format.as_str() {
            "wav" => matches!(
                self.codec.as_str(),
                "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_u8" | "pcm_f32le" | "pcm_f64le"
            ),
            "flac" => self.codec == "flac",
            "mp3" => matches!(self.codec.as_str(), "mp3" | "mp3float"),
            "ogg" => matches!(self.codec.as_str(), "opus" | "vorbis" | "flac"),
            "mov" => matches!(self.codec.as_str(), "aac" | "alac"),
            _ => false,
        };
        if !supported {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
impl Transcript {
    fn validate(&self, ms: u64, language: Option<&str>) -> Result<(), Error> {
        if !matches!(self.compute_type.as_str(), "int8" | "int8_float32")
            || !language_ok(&self.language)
            || language.is_some_and(|s| s != self.language)
            || !(0.0..=1.0).contains(&self.language_probability)
            || self.text.trim().is_empty()
            || self.segments.len() > 10000
        {
            return Err(Error::Protocol);
        }
        let mut prior = 0;
        let mut text = String::new();
        for s in &self.segments {
            if s.start_ms < prior
                || s.end_ms < s.start_ms
                || s.end_ms > ms
                || text.len() + s.text.len() > 1024 * 1024
            {
                return Err(Error::Protocol);
            }
            text.push_str(&s.text);
            prior = s.end_ms;
        }
        if text.trim() != self.text {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Client {
    python: PathBuf,
    model: PathBuf,
    slot: PathBuf,
}
impl Client {
    pub fn new(python: &Path, model: &Path, slot: &Path) -> Result<Self, Error> {
        if !python.is_absolute()
            || !python.is_file()
            || !model.is_absolute()
            || !model.is_dir()
            || !slot.is_absolute()
            || !slot.is_dir()
            || slot.is_symlink()
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            python: python.into(),
            model: model.into(),
            slot: slot.into(),
        })
    }
    pub async fn measure(
        &self,
        audio: &[u8],
        cancel: watch::Receiver<bool>,
    ) -> Result<Candidate, Error> {
        self.run(audio, None, None, cancel, None).await
    }
    pub async fn transcribe(
        &self,
        audio: &[u8],
        accepted: &Candidate,
        language: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Candidate, Error> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(Error::Cancelled);
        }
        if audio.len() > MAX_INPUT {
            return Err(Error::Limit);
        }
        if accepted.model_version != MODEL
            || accepted.transcript.is_some()
            || accepted.audio_sha256 != hex::encode(Sha256::digest(audio))
        {
            return Err(Error::Changed);
        }
        accepted.measure.validate()?;
        self.run(audio, Some(&accepted.measure), language, cancel, None)
            .await
    }
    async fn run(
        &self,
        audio: &[u8],
        measure: Option<&Measure>,
        language: Option<&str>,
        mut cancel: watch::Receiver<bool>,
        mut owner: Option<&mut owned::Attempt>,
    ) -> Result<Candidate, Error> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(Error::Cancelled);
        }
        if audio.len() > MAX_INPUT {
            return Err(Error::Limit);
        }
        if audio.is_empty() || language.is_some_and(|s| !language_ok(s)) {
            return Err(Error::Invalid);
        }
        let digest = hex::encode(Sha256::digest(audio));
        let pin: Value = serde_json::from_str(PIN).map_err(|_| Error::Protocol)?;
        let supported = pin["language_codes"].as_array().ok_or(Error::Protocol)?;
        if language.is_some_and(|code| !supported.iter().any(|v| v == code)) {
            return Err(Error::Invalid);
        }
        let frame=serde_json::to_vec(&json!({"protocol":1,"attempt_id":uuid::Uuid::new_v4().to_string(),"action":if measure.is_some(){"transcribe"}else{"measure"},"model_version":MODEL,"files":pin["files"],
            "audio_bytes":audio.len(),"audio_sha256":digest,"language":language,"expected_measure":measure})).map_err(|_|Error::Protocol)?;
        let mut process = Command::new(&self.python);
        process
            .args(["-I", "-B", "-c", RUNNER])
            .arg(&self.model)
            .arg(&self.slot)
            .env_clear()
            .env("HF_HUB_OFFLINE", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("OMP_NUM_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(a) = owner.as_ref() {
            process.env("ORANGE_ASR_INSTANCE", a.instance.to_string());
        }
        let mut child = process.spawn().map_err(|_| Error::Unavailable)?;
        let pid = child.id().ok_or(Error::Unavailable)?;
        let io = async {
            let mut out = BufReader::new(child.stdout.take().ok_or(Error::Unavailable)?);
            let mut first = Vec::new();
            (&mut out)
                .take(1025)
                .read_until(b'\n', &mut first)
                .await
                .map_err(|_| Error::Unavailable)?;
            let hello: Value = serde_json::from_slice(&first).map_err(|_| Error::Protocol)?;
            if let Some(a) = owner.as_deref_mut() {
                a.bind(&hello, &self.slot, pid)?;
            } else if hello != json!({"ready":true,"protocol":1}) {
                return Err(backend_error(&hello));
            }
            let mut input = child.stdin.take().ok_or(Error::Unavailable)?;
            input
                .write_all(&(frame.len() as u32).to_be_bytes())
                .await
                .map_err(|_| Error::Unavailable)?;
            input
                .write_all(&frame)
                .await
                .map_err(|_| Error::Unavailable)?;
            input
                .write_all(audio)
                .await
                .map_err(|_| Error::Unavailable)?;
            drop(input);
            let mut data = Vec::new();
            out.take(2097153)
                .read_to_end(&mut data)
                .await
                .map_err(|_| Error::Unavailable)?;
            if data.len() > 2097152 {
                return Err(Error::Limit);
            }
            if !child
                .wait()
                .await
                .map_err(|_| Error::Unavailable)?
                .success()
            {
                return Err(Error::Unavailable);
            }
            let v: Value = serde_json::from_slice(&data).map_err(|_| Error::Protocol)?;
            if v.get("error").is_some() {
                return Err(backend_error(&v));
            }
            let candidate: Candidate = serde_json::from_value(v).map_err(|_| Error::Protocol)?;
            if candidate.model_version != MODEL
                || candidate.audio_sha256 != digest
                || candidate.transcript.is_some() != measure.is_some()
            {
                return Err(Error::Protocol);
            }
            candidate.measure.validate()?;
            if measure.is_some_and(|expected| *expected != candidate.measure) {
                return Err(Error::Changed);
            }
            if let Some(t) = &candidate.transcript {
                t.validate(candidate.measure.duration_ms, language)?;
                if !supported.iter().any(|v| v == &t.language) {
                    return Err(Error::Protocol);
                }
            }
            Ok(candidate)
        };
        let deadline = Duration::from_secs(if measure.is_some() { 630 } else { 60 });
        let result = tokio::select! {biased;_=cancel.changed()=>Err(Error::Cancelled),v=tokio::time::timeout(deadline,io)=>v.unwrap_or(Err(Error::Timeout))};
        if result.is_err() && child.try_wait().map_err(|_| Error::Unavailable)?.is_none() {
            child.kill().await.map_err(|_| Error::Unavailable)?;
        }
        if let Some(a) = owner
            && a.bound()
        {
            let status = child
                .try_wait()
                .map_err(|_| Error::Unavailable)?
                .ok_or(Error::Unavailable)?;
            a.reaped(&self.slot, pid, status)?;
        }
        result
    }
}
fn backend_error(v: &Value) -> Error {
    match v["error"].as_str() {
        Some("limit") => Error::Limit,
        Some("invalid_audio_request") => Error::Invalid,
        Some("no_speech") => Error::NoSpeech,
        Some("slot_busy") => Error::Busy,
        Some("measurement_changed") => Error::Changed,
        Some("transcription_backend_unavailable") => Error::Unavailable,
        _ => Error::Protocol,
    }
}
pub fn read_audio(path: &Path) -> Result<Vec<u8>, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| Error::Invalid)?;
    let m = f.metadata().map_err(|_| Error::Invalid)?;
    if !m.is_file() || m.len() == 0 {
        return Err(Error::Invalid);
    }
    if m.len() > MAX_INPUT as u64 {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    Read::take(f, MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Invalid)?;
    if bytes.len() as u64 != m.len() {
        return Err(Error::Changed);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transcript() -> Transcript {
        Transcript {
            compute_type: "int8_float32".into(),
            language: "en".into(),
            language_probability: 0.99,
            text: "one two".into(),
            segments: vec![
                Segment {
                    start_ms: 0,
                    end_ms: 300,
                    text: " one".into(),
                },
                Segment {
                    start_ms: 300,
                    end_ms: 900,
                    text: " two".into(),
                },
            ],
        }
    }
    #[test]
    fn transcript_requires_complete_ordered_timing_and_exact_text() {
        transcript()
            .validate(900, Some("en"))
            .expect("complete native segments");
        assert!(transcript().validate(899, None).is_err());
        assert!(transcript().validate(900, Some("zh")).is_err());
        let mut t = transcript();
        t.segments[1].start_ms = 200;
        assert!(t.validate(900, None).is_err());
        let mut t = transcript();
        t.text.push('x');
        assert!(t.validate(900, None).is_err());
        let mut t = transcript();
        t.language_probability = f64::NAN;
        assert!(t.validate(900, None).is_err());
    }
    #[test]
    fn measurements_bind_one_rounding_operation_and_both_sample_counts() {
        let mut m = Measure {
            preprocessing: "mono-16khz-s16-f32-v1".into(),
            format: "wav".into(),
            codec: "pcm_s16le".into(),
            sample_rate: 16000,
            source_samples: 17,
            normalized_samples: 17,
            normalized_sha256: "d".repeat(64),
            duration_ms: 2,
        };
        m.validate().expect("submillisecond remainder");
        m.duration_ms = 1;
        assert!(m.validate().is_err());
        m.duration_ms = 2;
        m.normalized_samples = 1;
        assert!(m.validate().is_err());
    }
}
