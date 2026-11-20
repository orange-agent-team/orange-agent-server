#![cfg(unix)]
use orange_model_runtime::transcription::{Client, read_audio};
use sha2::{Digest, Sha256};
use std::{env, fs::File, path::PathBuf};
use tokio::sync::watch;

#[tokio::test]
#[ignore = "requires the pinned local Python environment and full Whisper model"]
async fn actual_child_exit_retains_exclusive_slot_until_consumed() {
    let python = PathBuf::from(env::var_os("ORANGE_ASR_TEST_PYTHON").expect("pinned Python"));
    let model = PathBuf::from(env::var_os("ORANGE_ASR_TEST_MODEL").expect("pinned Whisper"));
    let slot = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .tempdir()
        .expect("private slot");
    let client = Client::new(&python, &model, slot.path()).expect("client");
    let (_sender, cancel) = watch::channel(false);
    let fact = client
        .qualify(None, cancel.clone())
        .await
        .expect("qualified runtime");
    let audio = read_audio(&model.join("jfk.wav")).expect("recording");
    let measured = client
        .measure(&audio, cancel.clone())
        .await
        .expect("decoder");
    let instance = uuid::Uuid::new_v4();
    let run = client
        .transcribe_owned(
            &audio,
            &measured,
            Some("en"),
            &fact,
            instance,
            cancel.clone(),
        )
        .await
        .expect("owned run");
    assert!(run.holds_slot(slot.path()));
    assert_eq!(run.process.instance_id, instance.to_string());
    assert_eq!(
        run.process.executable_sha256,
        fact.profile.runtime.python_sha256
    );
    assert_eq!(run.process.exit_code, Some(0));
    assert!(run.process.pid.is_some_and(|pid| pid > 0));
    let candidate = run.result.as_ref().expect("transcript");
    assert_eq!(
        hex::encode(Sha256::digest(
            candidate
                .transcript
                .as_ref()
                .expect("speech")
                .text
                .as_bytes()
        )),
        fact.transcript_sha256
    );
    let other = File::options()
        .read(true)
        .write(true)
        .open(slot.path().join("transcription.lock"))
        .expect("second description");
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    // A second owner cannot run or borrow the retired process's evidence.
    assert!(
        client
            .transcribe_owned(&audio, &measured, None, &fact, uuid::Uuid::new_v4(), cancel)
            .await
            .is_err()
    );
    drop(run);
    other
        .try_lock()
        .expect("slot released by the owning witness");
}
