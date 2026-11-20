//! Pull verified audio ranges under a live local start grant. The execution owner may renew
//! the lease between chunks. Only a complete, digest-checked recording is exposed.
use super::{execution::StartedExecution, *};
use crate::execution_journal::ExecutionJournal;
use orange_model_runtime::transcription::{Candidate, MODEL, Measure};
use orange_protocol::pricing::{AUDIO_INPUT_CHUNK_BYTES, AudioMeasurement};
use sha2::{Digest, Sha256};

pub struct TranscriptionDownload {
    grant_id: String,
    assignment_digest: String,
    projection: String,
    profile: String,
    buffer: Vec<u8>,
    first: Option<(AudioMeasurement, Option<String>, usize)>,
}
pub struct VerifiedTranscriptionInput {
    measurement: AudioMeasurement,
    projection: String,
    profile: String,
    pub audio: Vec<u8>,
    pub accepted: Candidate,
    pub language: Option<String>,
}
impl TranscriptionDownload {
    pub fn for_record(
        record: &crate::execution_journal::ExecutionRecord,
    ) -> Result<Self, ControlError> {
        let input = record
            .assignment
            .transcription_input
            .as_ref()
            .ok_or(ControlError::Protocol)?;
        Self::new(&input.grant_id, &record.assignment)
    }
    fn new(grant_id: &str, assignment: &worker::Assignment) -> Result<Self, ControlError> {
        if !orange_protocol::valid_id(grant_id)
            || assignment.model_version_id != MODEL
            || orange_protocol::execution::assignment_digest(assignment).as_deref()
                != Some(assignment.payload_digest.as_str())
            || !assignment.input_artifacts.is_empty()
            || !assignment.input_artifact_ids.is_empty()
        {
            return Err(ControlError::Protocol);
        }
        Ok(Self {
            grant_id: grant_id.into(),
            assignment_digest: assignment.payload_digest.clone(),
            projection: assignment.request_projection_digest.clone(),
            profile: assignment.execution_profile_digest.clone(),
            buffer: Vec::new(),
            first: None,
        })
    }

    fn accept(&mut self, reply: worker::TranscriptionInputChunk) -> Result<bool, ControlError> {
        let start = self.buffer.len();
        let size = usize::try_from(reply.size_bytes).map_err(|_| ControlError::Protocol)?;
        if !(1..=orange_model_runtime::transcription::MAX_INPUT).contains(&size)
            || start >= size
            || reply.version != 1
            || reply.grant_id != self.grant_id
            || reply.projection_digest != self.projection
            || reply.offset != start as u64
            || reply.data.len() != (size - start).min(AUDIO_INPUT_CHUNK_BYTES)
            || reply.eof != (start + reply.data.len() == size)
            || hex::encode(Sha256::digest(&reply.data)) != reply.chunk_sha256
        {
            return Err(ControlError::Protocol);
        }
        if start == 0 {
            if reply.measurement_json.len() > 8192
                || !reply.language.is_empty()
                    && !(reply.language.len() <= 3
                        && reply.language.len() >= 2
                        && reply.language.bytes().all(|b| b.is_ascii_lowercase()))
            {
                return Err(ControlError::Protocol);
            }
            let m: AudioMeasurement = serde_json::from_slice(&reply.measurement_json)
                .map_err(|_| ControlError::Protocol)?;
            if m.model_version != MODEL
                || !orange_protocol::valid_id(&m.artifact_id)
                || !orange_protocol::valid_id(&m.version_id)
                || !orange_protocol::valid_digest(&m.audio_sha256)
                || !orange_protocol::valid_digest(&m.normalized_sha256)
                || m.artifact_id != reply.artifact_id
                || m.version_id != reply.version_id
                || m.audio_sha256 != reply.audio_sha256
            {
                return Err(ControlError::Protocol);
            }
            self.first = Some((
                m,
                (!reply.language.is_empty()).then_some(reply.language),
                size,
            ));
        } else if !reply.measurement_json.is_empty()
            || !reply.language.is_empty()
            || !self.first.as_ref().is_some_and(|(m, _, n)| {
                *n == size
                    && m.artifact_id == reply.artifact_id
                    && m.version_id == reply.version_id
                    && m.audio_sha256 == reply.audio_sha256
            })
        {
            return Err(ControlError::Protocol);
        }
        self.buffer.extend_from_slice(&reply.data);
        Ok(reply.eof)
    }

    pub fn finish(self) -> Result<VerifiedTranscriptionInput, ControlError> {
        let (m, language, size) = self.first.ok_or(ControlError::Protocol)?;
        if self.buffer.len() != size || hex::encode(Sha256::digest(&self.buffer)) != m.audio_sha256
        {
            return Err(ControlError::Protocol);
        }
        let count = |s: &str| -> Result<u64, ControlError> {
            let v = s.parse::<u64>().map_err(|_| ControlError::Protocol)?;
            if v == 0 || v.to_string() != s {
                return Err(ControlError::Protocol);
            }
            Ok(v)
        };
        let original = m.clone();
        let accepted = Candidate {
            model_version: m.model_version,
            audio_sha256: m.audio_sha256,
            transcript: None,
            measure: Measure {
                preprocessing: m.preprocessing,
                format: m.format,
                codec: m.codec,
                sample_rate: m.source_sample_rate,
                source_samples: count(&m.source_samples)?,
                normalized_samples: count(&m.normalized_samples)?,
                normalized_sha256: m.normalized_sha256,
                duration_ms: count(&m.duration_ms)?,
            },
        };
        accepted
            .measure
            .validate()
            .map_err(|_| ControlError::Protocol)?;
        Ok(VerifiedTranscriptionInput {
            measurement: original,
            projection: self.projection,
            profile: self.profile,
            audio: self.buffer,
            accepted,
            language,
        })
    }
}
impl VerifiedTranscriptionInput {
    pub fn output(
        &self,
        candidate: Candidate,
        expected_compute: &str,
    ) -> Result<orange_protocol::transcript::TranscriptionResult, ControlError> {
        if candidate.model_version != self.accepted.model_version
            || candidate.audio_sha256 != self.accepted.audio_sha256
            || candidate.measure != self.accepted.measure
        {
            return Err(ControlError::Protocol);
        }
        let t = candidate.transcript.ok_or(ControlError::Protocol)?;
        let result = orange_protocol::transcript::TranscriptionResult {
            version: 1,
            request_projection_digest: self.projection.clone(),
            execution_profile_digest: self.profile.clone(),
            measurement: self.measurement.clone(),
            compute_type: t.compute_type,
            language: t.language,
            language_probability: t.language_probability,
            text: t.text,
            segments: t
                .segments
                .into_iter()
                .map(|v| orange_protocol::transcript::TimedText {
                    start_ms: v.start_ms,
                    end_ms: v.end_ms,
                    text: v.text,
                })
                .collect(),
        };
        if !result.matches(
            &self.measurement,
            &self.projection,
            &self.profile,
            expected_compute,
            self.language.as_deref(),
        ) {
            return Err(ControlError::Protocol);
        }
        Ok(result)
    }
}
impl ControlConnection {
    /// Negotiates audio input references and independently checked execution renewals.
    pub async fn connect_transcription_inputs(
        config: ControlConfig,
        node: &mut NodeJournal,
        journal: &mut ExecutionJournal,
    ) -> Result<Self, ControlError> {
        Self::connect_execution_mode(
            config,
            node,
            journal,
            ControlVersions {
                rejection_receipts_version: 1,
                input_artifacts_version: 1,
                renewal_version: 1,
                transcription_input_version: 1,
                ..ControlVersions::default()
            },
        )
        .await
    }
    pub async fn read_transcription_chunk(
        &mut self,
        journal: &ExecutionJournal,
        started: &StartedExecution,
        command: &str,
        download: &mut TranscriptionDownload,
    ) -> Result<bool, ControlError> {
        if self.transcription_input_version != 1
            || self.input_artifacts_version != 1
            || !self.is_current_locally()
        {
            return Err(ControlError::Stale);
        }
        let record = journal
            .record(command)
            .map_err(|_| ControlError::Unavailable)?
            .ok_or(ControlError::Invalid)?;
        if record.state != "running"
            || record.grant.as_ref() != Some(started.grant())
            || record.assignment.session_id != self.identity.session_id
            || record.assignment.payload_digest != download.assignment_digest
        {
            return Err(ControlError::Stale);
        }
        let end = self
            .stop_at
            .min(started.stop_at())
            .min(Instant::now() + Duration::from_secs(5));
        if Instant::now() >= end {
            return Err(ControlError::Stale);
        }
        let request = worker::TranscriptionInputRead {
            version: 1,
            session_id: self.identity.session_id.clone(),
            lease: record.assignment.lease,
            grant_id: download.grant_id.clone(),
            projection_digest: download.projection.clone(),
            offset: download.buffer.len() as u64,
        };
        let reply = tokio::time::timeout_at(
            tokio::time::Instant::from_std(end),
            self.input_client.read_transcription_input(request),
        )
        .await
        .map_err(|_| ControlError::Unavailable)?
        .map_err(status)?
        .into_inner();
        if Instant::now() >= end || !self.is_current_locally() {
            return Err(ControlError::Stale);
        }
        download.accept(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> TranscriptionDownload {
        TranscriptionDownload {
            grant_id: "grant".into(),
            assignment_digest: "a".repeat(64),
            projection: "b".repeat(64),
            profile: "c".repeat(64),
            buffer: Vec::new(),
            first: None,
        }
    }
    fn chunk(bytes: &[u8], start: usize) -> worker::TranscriptionInputChunk {
        let data = bytes[start..(start + AUDIO_INPUT_CHUNK_BYTES).min(bytes.len())].to_vec();
        let sha = hex::encode(Sha256::digest(bytes));
        let measurement = AudioMeasurement {
            model_version: MODEL.into(),
            artifact_id: "artifact".into(),
            version_id: "version".into(),
            audio_sha256: sha.clone(),
            preprocessing: "mono-16khz-s16-f32-v1".into(),
            format: "wav".into(),
            codec: "pcm_s16le".into(),
            source_sample_rate: 16000,
            source_samples: "176000".into(),
            normalized_samples: "176000".into(),
            normalized_sha256: "a".repeat(64),
            duration_ms: "11000".into(),
        };
        worker::TranscriptionInputChunk {
            version: 1,
            grant_id: "grant".into(),
            projection_digest: "b".repeat(64),
            artifact_id: "artifact".into(),
            version_id: "version".into(),
            audio_sha256: sha,
            size_bytes: bytes.len() as u64,
            offset: start as u64,
            eof: start + data.len() == bytes.len(),
            chunk_sha256: hex::encode(Sha256::digest(&data)),
            data,
            language: if start == 0 {
                "en".into()
            } else {
                String::new()
            },
            measurement_json: if start == 0 {
                serde_json::to_vec(&measurement).expect("fixture")
            } else {
                vec![]
            },
        }
    }
    #[test]
    fn partial_mixed_and_truncated_ranges_never_finish() {
        let src = vec![7; AUDIO_INPUT_CHUNK_BYTES + 17];
        let start = chunk(&src, 0);
        let end = chunk(&src, AUDIO_INPUT_CHUNK_BYTES);
        for field in 0..6 {
            let mut c = start.clone();
            match field {
                0 => c.eof = true,
                1 => c.offset = 1,
                2 => c.grant_id = "other".into(),
                3 => c.data[0] ^= 1,
                4 => c.size_bytes = u64::MAX,
                _ => c.measurement_json.clear(),
            }
            assert!(empty().accept(c).is_err());
        }
        let mut partial = empty();
        assert!(!partial.accept(start.clone()).expect("first"));
        assert!(partial.finish().is_err());
        let mut transfer = empty();
        transfer.accept(start).expect("first");
        let mut mixed = end.clone();
        mixed.version_id = "other".into();
        assert!(transfer.accept(mixed).is_err());
        let mut truncated = end.clone();
        truncated.data.pop();
        truncated.chunk_sha256 = hex::encode(Sha256::digest(&truncated.data));
        assert!(transfer.accept(truncated).is_err());
        let mut altered = end;
        altered.data[0] ^= 1;
        altered.chunk_sha256 = hex::encode(Sha256::digest(&altered.data));
        assert!(
            transfer
                .accept(altered)
                .expect("self-consistent final chunk")
        );
        assert!(transfer.finish().is_err(), "full input hash must match");
    }
}
