//! Explicit integration-test scheduling only. No gate can forge a response or
//! alter authorization; this module is absent from default Worker builds.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};
use tokio::sync::watch;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    BeforeHeartbeat,
    AfterHeartbeat,
}
struct Entry {
    stage: Stage,
    release: watch::Sender<bool>,
    reached: watch::Sender<bool>,
}
static GATES: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub struct Gate {
    command: String,
    reached: watch::Receiver<bool>,
}
impl Gate {
    pub fn install(command: &str, stage: Stage) -> Self {
        assert!(orange_protocol::valid_id(command));
        let (release, _) = watch::channel(false);
        let (reached, receiver) = watch::channel(false);
        assert!(
            GATES
                .lock()
                .expect("test gate registry")
                .insert(
                    command.into(),
                    Entry {
                        stage,
                        release,
                        reached
                    }
                )
                .is_none(),
            "unique owned command gate"
        );
        Self {
            command: command.into(),
            reached: receiver,
        }
    }
    pub async fn reached(&mut self) {
        while !*self.reached.borrow() {
            self.reached.changed().await.expect("live test gate");
        }
    }
    pub fn release(&self) {
        if let Some(entry) = GATES.lock().expect("test gate registry").get(&self.command) {
            entry.release.send_replace(true);
        }
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        if let Some(entry) = GATES
            .lock()
            .expect("test gate registry")
            .remove(&self.command)
        {
            entry.release.send_replace(true);
        }
    }
}
pub(super) async fn wait(command: &str, stage: Stage) {
    let receiver = {
        GATES
            .lock()
            .expect("test gate registry")
            .get(command)
            .filter(|e| e.stage == stage)
            .map(|e| {
                e.reached.send_replace(true);
                e.release.subscribe()
            })
    };
    if let Some(mut receiver) = receiver {
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
}
