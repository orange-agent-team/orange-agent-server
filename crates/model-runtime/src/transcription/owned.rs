//! Bind a specific child process to its exclusive kernel slot before sending audio.
use super::*;
use crate::supervisor::ProcessRecord;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::process::ExitStatus;
use uuid::Uuid;

/// The lock keeps the exact retired slot unavailable until the Broker has recorded release.
pub struct OwnedTranscription {
    pub result: Result<Candidate, Error>,
    pub process: ProcessRecord,
    slot: File,
}
impl OwnedTranscription {
    pub fn holds_slot(&self, root: &Path) -> bool {
        matches!((self.slot.metadata(),std::fs::symlink_metadata(root.join("transcription.lock"))),
            (Ok(fd),Ok(path)) if private(&fd) && private(&path) && fd.dev()==path.dev() && fd.ino()==path.ino())
    }
}

fn private(meta: &std::fs::Metadata) -> bool {
    meta.is_file()
        && meta.nlink() == 1
        && meta.mode() & 0o077 == 0
        && meta.uid() == rustix::process::geteuid().as_raw()
}

pub(super) struct Attempt {
    pub instance: Uuid,
    executable: String,
    model: String,
    slot: Option<File>,
    stopped: Option<ProcessRecord>,
}
impl Attempt {
    pub(super) fn bind(&mut self, hello: &Value, root: &Path, pid: u32) -> Result<(), Error> {
        if hello.get("error").is_some() {
            return Err(backend_error(hello));
        }
        let slot = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("transcription.lock"))
            .map_err(|_| Error::Changed)?;
        let info = slot.metadata().map_err(|_| Error::Changed)?;
        if !private(&info)
            || hello
                != &json!({"ready":true,"protocol":1,"instance":self.instance.to_string(),"pid":pid,
            "python_sha256":self.executable,"slot_dev":info.dev().to_string(),"slot_ino":info.ino().to_string()})
        {
            return Err(Error::Changed);
        }
        match slot.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => {
                self.slot = Some(slot);
                Ok(())
            }
            _ => Err(Error::Changed),
        }
    }
    pub(super) fn reaped(
        &mut self,
        root: &Path,
        pid: u32,
        status: ExitStatus,
    ) -> Result<(), Error> {
        let fd = self.slot.as_ref().ok_or(Error::Unavailable)?;
        fd.try_lock().map_err(|_| Error::Busy)?;
        let held = fd.metadata().map_err(|_| Error::Changed)?;
        let actual = std::fs::symlink_metadata(root.join("transcription.lock"))
            .map_err(|_| Error::Changed)?;
        if !private(&held)
            || !private(&actual)
            || held.dev() != actual.dev()
            || held.ino() != actual.ino()
        {
            return Err(Error::Changed);
        }
        self.stopped = Some(ProcessRecord {
            instance_id: self.instance.to_string(),
            model_version: MODEL.into(),
            executable_sha256: self.executable.clone(),
            model_sha256: self.model.clone(),
            phase: "stopped".into(),
            pid: Some(pid),
            exit_code: status.code(),
            cpu_only: true,
        });
        Ok(())
    }
}
impl Client {
    pub async fn transcribe_owned(
        &self,
        audio: &[u8],
        accepted: &Candidate,
        language: Option<&str>,
        qualified: &Qualification,
        instance: Uuid,
        cancel: watch::Receiver<bool>,
    ) -> Result<OwnedTranscription, Error> {
        if accepted.model_version != MODEL
            || accepted.transcript.is_some()
            || audio.len() > MAX_INPUT
            || accepted.audio_sha256 != hex::encode(Sha256::digest(audio))
            || qualified.profile.device != "cpu"
            || qualified.profile.runner_sha256 != hex::encode(Sha256::digest(RUNNER.as_bytes()))
            || orange_protocol::request_digest(
                "worker.transcription-profile.v1",
                &qualified.profile,
            )
            .map_err(|_| Error::Invalid)?
                != qualified.execution_profile_digest
        {
            return Err(Error::Changed);
        }
        accepted.measure.validate()?;
        // The currently installed runtime and loaded dependency files must still match the observation.
        if self.observe(cancel.clone()).await? != qualified.profile.runtime {
            return Err(Error::Changed);
        }
        let mut a = Attempt {
            instance,
            executable: qualified.profile.runtime.python_sha256.clone(),
            model: qualified.profile.model_sha256.clone(),
            slot: None,
            stopped: None,
        };
        let result = self
            .run(
                audio,
                Some(&accepted.measure),
                language,
                cancel,
                Some(&mut a),
            )
            .await;
        match (a.stopped, a.slot) {
            (Some(process), Some(slot)) => Ok(OwnedTranscription {
                result,
                process,
                slot,
            }),
            _ => Err(result.err().unwrap_or(Error::Unavailable)),
        }
    }
}
