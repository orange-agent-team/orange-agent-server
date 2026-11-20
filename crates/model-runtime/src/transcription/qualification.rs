//! Local observation of a fixed ASR profile; remote admission is a separate decision.
use super::*;
use std::collections::BTreeMap;
const PROBE: &str = include_str!("../transcription_probe.py");
const ENVIRONMENT: &[u8] = include_bytes!("../../../../config/transcription-probe.v1.json");
const VERIFIER: &[u8] =
    include_bytes!("../../../../backends/orange_inference_backends/embedding.py");

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedRuntime {
    pub target: String,
    pub python_version: String,
    pub python_abi: String,
    pub python_sha256: String,
    pub packages: BTreeMap<String, String>,
    pub modules: BTreeMap<String, String>,
}
#[derive(Debug, Serialize)]
pub struct ExecutionProfile {
    pub model_version: &'static str,
    pub model_sha256: String,
    pub model_profile_sha256: String,
    pub runner_sha256: String,
    pub device: &'static str,
    pub compute_type: String,
    pub preprocessing: String,
    pub cpu_threads: u8,
    pub workers: u8,
    pub max_input_bytes: usize,
    pub max_duration_ms: u64,
    pub runtime: ObservedRuntime,
}
#[derive(Debug, Serialize)]
pub struct Qualification {
    pub protocol: u8,
    pub execution_profile_digest: String,
    pub profile: ExecutionProfile,
    pub sample_sha256: String,
    pub sample_duration_ms: u64,
    pub normalized_samples: u64,
    pub transcript_sha256: String,
}

impl Client {
    pub(super) async fn observe(
        &self,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<ObservedRuntime, Error> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(Error::Cancelled);
        }
        let mut child = Command::new(&self.python)
            .args(["-I", "-B", "-c", PROBE])
            .env_clear()
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::Unavailable)?;
        let read = async {
            let mut input = child.stdin.take().ok_or(Error::Unavailable)?;
            input
                .write_all(ENVIRONMENT)
                .await
                .map_err(|_| Error::Unavailable)?;
            drop(input);
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or(Error::Unavailable)?
                .take(32769)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
            if bytes.len() > 32768 {
                return Err(Error::Protocol);
            }
            if !child
                .wait()
                .await
                .map_err(|_| Error::Unavailable)?
                .success()
            {
                return Err(Error::Unavailable);
            }
            let observed: ObservedRuntime =
                serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            let expected: Value =
                serde_json::from_slice(ENVIRONMENT).map_err(|_| Error::Protocol)?;
            let names = expected["critical_modules"]
                .as_array()
                .ok_or(Error::Protocol)?;
            if observed.target != format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
                || observed.target != expected["target"]
                || !observed.python_version.starts_with("3.12.")
                || !observed.python_abi.starts_with("cpython-312-")
                || !orange_protocol::valid_digest(&observed.python_sha256)
                || serde_json::to_value(&observed.packages).map_err(|_| Error::Protocol)?
                    != expected["packages"]
                || observed.modules.len() != names.len()
                || !names.iter().all(|k| {
                    k.as_str()
                        .and_then(|s| observed.modules.get(s))
                        .is_some_and(|h| orange_protocol::valid_digest(h))
                })
                || observed.modules.get("orange_inference_backends.embedding")
                    != Some(&hex::encode(Sha256::digest(VERIFIER)))
            {
                return Err(Error::Protocol);
            }
            Ok(observed)
        };
        let result = tokio::select! {biased;_=cancel.changed()=>Err(Error::Cancelled), r=tokio::time::timeout(Duration::from_secs(30),read)=>r.unwrap_or(Err(Error::Timeout))};
        if result.is_err() && child.try_wait().map_err(|_| Error::Unavailable)?.is_none() {
            child.kill().await.map_err(|_| Error::Unavailable)?;
        }
        result
    }

    /// Run the approved public qualification recording, independent of any user artifact, Job or wallet.
    pub async fn qualify(
        &self,
        expected_digest: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Qualification, Error> {
        if expected_digest.is_some_and(|s| !orange_protocol::valid_digest(s)) {
            return Err(Error::Invalid);
        }
        let runtime = self.observe(cancel.clone()).await?;
        let encoded = read_audio(&self.model.join("jfk.wav"))?;
        let pin: Value = serde_json::from_str(PIN).map_err(|_| Error::Protocol)?;
        let sample = &pin["qualification_sample"];
        let sha = hex::encode(Sha256::digest(&encoded));
        if sha != sample["sha256"] || Some(encoded.len() as u64) != sample["bytes"].as_u64() {
            return Err(Error::Changed);
        }
        let measured = self.measure(&encoded, cancel.clone()).await?;
        let result = self
            .transcribe(&encoded, &measured, None, cancel.clone())
            .await?;
        let t = result.transcript.ok_or(Error::Protocol)?;
        let words: Vec<_> = t
            .text
            .split(|c: char| !c.is_ascii_alphabetic())
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if words.join(" ")
            != "and so my fellow americans ask not what your country can do for you ask what you can do for your country"
            || t.language != "en"
            || t.segments.is_empty()
            || Some(result.measure.duration_ms) != sample["duration_ms"].as_u64()
            || Some(result.measure.normalized_samples) != sample["samples"].as_u64()
        {
            return Err(Error::Protocol);
        }
        if runtime != self.observe(cancel).await? {
            return Err(Error::Changed);
        }
        let profile = ExecutionProfile {
            model_version: MODEL,
            model_sha256: pin["files"]["model.bin"]["sha256"]
                .as_str()
                .ok_or(Error::Protocol)?
                .to_owned(),
            model_profile_sha256: hex::encode(Sha256::digest(PIN.as_bytes())),
            runner_sha256: hex::encode(Sha256::digest(RUNNER.as_bytes())),
            device: "cpu",
            compute_type: t.compute_type,
            preprocessing: result.measure.preprocessing,
            cpu_threads: 2,
            workers: 1,
            max_input_bytes: MAX_INPUT,
            max_duration_ms: pin["max_decoded_ms"].as_u64().ok_or(Error::Protocol)?,
            runtime,
        };
        let digest = orange_protocol::request_digest("worker.transcription-profile.v1", &profile)
            .map_err(|_| Error::Protocol)?;
        if expected_digest.is_some_and(|s| s != digest) {
            return Err(Error::Changed);
        }
        Ok(Qualification {
            protocol: 1,
            execution_profile_digest: digest,
            profile,
            sample_sha256: sha,
            sample_duration_ms: result.measure.duration_ms,
            normalized_samples: result.measure.normalized_samples,
            transcript_sha256: hex::encode(Sha256::digest(t.text.as_bytes())),
        })
    }
}
