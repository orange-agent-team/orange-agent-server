//! Durable receipt of authenticated node stop intents. No method here executes a
//! process, changes Broker occupancy or claims physical resource cleanup.
use crate::{
    broker::{BrokerError, storage},
    control::ControlIdentity,
};
use orange_protocol::{node_control::NodeStopIntent, valid_digest, valid_id};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("invalid node command or journal configuration")]
    Invalid,
    #[error("node control session or policy generation is stale")]
    Stale,
    #[error("node command identity or payload conflicts with durable history")]
    Conflict,
    #[error("unsupported or unexpected node journal format")]
    Format,
    #[error(transparent)]
    Storage(#[from] BrokerError),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedNodeIntent {
    pub intent: NodeStopIntent,
    pub payload_digest: String,
    pub receipt_id: String,
}
pub struct NodeJournal {
    connection: Connection,
    _lock: File,
    node_id: String,
}
impl NodeJournal {
    pub fn open(path: &Path, node_id: String) -> Result<Self, JournalError> {
        if !valid_id(&node_id)
            || !node_id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(JournalError::Invalid);
        }
        let (mut connection, lock) = storage::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(0..=1).contains(&version) {
            return Err(JournalError::Format);
        }
        if version == 0 {
            let count:i64=connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
            if count != 0 {
                return Err(JournalError::Format);
            }
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=100;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            tx.execute_batch("CREATE TABLE node_receiver(singleton INTEGER PRIMARY KEY CHECK(singleton=1),node_id TEXT NOT NULL,connection_epoch INTEGER NOT NULL DEFAULT 0,session_id TEXT NOT NULL DEFAULT '',policy_generation INTEGER NOT NULL DEFAULT 0); CREATE TABLE node_intents(command_id TEXT PRIMARY KEY,payload_digest TEXT NOT NULL,body TEXT NOT NULL,receipt_id TEXT NOT NULL UNIQUE,policy_generation INTEGER NOT NULL UNIQUE CHECK(policy_generation>0),connection_epoch INTEGER NOT NULL,session_id TEXT NOT NULL); CREATE TRIGGER node_intents_no_update BEFORE UPDATE ON node_intents BEGIN SELECT RAISE(ABORT,'immutable node intent'); END; CREATE TRIGGER node_intents_no_delete BEFORE DELETE ON node_intents BEGIN SELECT RAISE(ABORT,'immutable node intent'); END; PRAGMA user_version=1;")?;
            tx.execute(
                "INSERT INTO node_receiver(singleton,node_id) VALUES(1,?)",
                [&node_id],
            )?;
        } else {
            let existing: String = tx.query_row(
                "SELECT node_id FROM node_receiver WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if existing != node_id {
                return Err(JournalError::Conflict);
            }
        }
        tx.commit()?;
        Ok(Self {
            connection,
            _lock: lock,
            node_id,
        })
    }
    pub fn node_id(&self) -> &str {
        &self.node_id
    }
    pub fn latest_received(&self) -> Result<Option<ReceivedNodeIntent>, JournalError> {
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT command_id FROM node_intents ORDER BY policy_generation DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.received(&id))
            .transpose()
            .map(Option::flatten)
    }
    pub(crate) fn current_receipt(
        &self,
        identity: &ControlIdentity,
        command: &str,
    ) -> Result<ReceivedNodeIntent, JournalError> {
        let row:Option<(i64,String)>=self.connection.query_row("SELECT i.connection_epoch,i.session_id FROM node_intents i JOIN node_receiver r ON r.singleton=1 WHERE i.command_id=? AND i.policy_generation=r.policy_generation AND r.node_id=? AND r.connection_epoch=? AND r.session_id=?",params![command,identity.node_id,i64::try_from(identity.connection_epoch).map_err(|_|JournalError::Invalid)?,identity.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (epoch, session) = row.ok_or(JournalError::Stale)?;
        if epoch as u64 != identity.connection_epoch || session != identity.session_id {
            return Err(JournalError::Stale);
        }
        self.received(command)?.ok_or(JournalError::Stale)
    }
    /// Only the in-crate mTLS control adapter can bind a current connection. Public
    /// callers cannot acknowledge arbitrary deserialized commands through this API.
    pub(crate) fn bind(&mut self, identity: &ControlIdentity) -> Result<(), JournalError> {
        if identity.node_id != self.node_id
            || !valid_id(&identity.session_id)
            || identity.connection_epoch == 0
        {
            return Err(JournalError::Invalid);
        }
        let epoch = i64::try_from(identity.connection_epoch).map_err(|_| JournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (old, session): (i64, String) = tx.query_row(
            "SELECT connection_epoch,session_id FROM node_receiver WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if old > epoch || (old == epoch && session != identity.session_id) {
            return Err(JournalError::Stale);
        }
        tx.execute(
            "UPDATE node_receiver SET connection_epoch=?,session_id=? WHERE singleton=1",
            params![epoch, identity.session_id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn receive(
        &mut self,
        identity: &ControlIdentity,
        intent: &NodeStopIntent,
        payload_digest: &str,
    ) -> Result<ReceivedNodeIntent, JournalError> {
        if identity.node_id != self.node_id
            || intent.node_id != self.node_id
            || !intent.validate()
            || !valid_digest(payload_digest)
            || intent.digest().map_err(|_| JournalError::Invalid)? != payload_digest
        {
            return Err(JournalError::Invalid);
        }
        let epoch = i64::try_from(identity.connection_epoch).map_err(|_| JournalError::Invalid)?;
        let generation = intent.generation().ok_or(JournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_epoch,current_session,current_generation):(i64,String,i64)=tx.query_row("SELECT connection_epoch,session_id,policy_generation FROM node_receiver WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if epoch <= 0
            || current_epoch != epoch
            || current_session != identity.session_id
            || generation < current_generation
        {
            return Err(JournalError::Stale);
        }
        let previous: Option<(String, String, String)> = tx
            .query_row(
                "SELECT payload_digest,body,receipt_id FROM node_intents WHERE command_id=?",
                [&intent.command_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let receipt_id = if let Some((digest, body, receipt)) = previous {
            let stored: NodeStopIntent =
                serde_json::from_str(&body).map_err(|_| JournalError::Format)?;
            if digest != payload_digest || stored != *intent {
                return Err(JournalError::Conflict);
            }
            receipt
        } else {
            if generation <= current_generation {
                return Err(JournalError::Conflict);
            }
            let receipt = uuid::Uuid::new_v4().to_string();
            let body = serde_json::to_string(intent).map_err(|_| JournalError::Invalid)?;
            tx.execute("INSERT INTO node_intents(command_id,payload_digest,body,receipt_id,policy_generation,connection_epoch,session_id) VALUES(?,?,?,?,?,?,?)",params![intent.command_id,payload_digest,body,receipt,generation,epoch,identity.session_id])?;
            tx.execute(
                "UPDATE node_receiver SET policy_generation=? WHERE singleton=1",
                [generation],
            )?;
            receipt
        };
        tx.commit()?;
        Ok(ReceivedNodeIntent {
            intent: intent.clone(),
            payload_digest: payload_digest.into(),
            receipt_id,
        })
    }
    /// Read-only recovery evidence. Existence proves durable receipt, never apply
    /// or cleanup, and must not be used to replay an uncertain external effect.
    pub fn received(&self, command_id: &str) -> Result<Option<ReceivedNodeIntent>, JournalError> {
        if !valid_id(command_id) {
            return Err(JournalError::Invalid);
        }
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT body,payload_digest,receipt_id FROM node_intents WHERE command_id=?",
                [command_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(body, payload_digest, receipt_id)| {
            let intent: NodeStopIntent =
                serde_json::from_str(&body).map_err(|_| JournalError::Format)?;
            if !intent.validate()
                || intent.node_id != self.node_id
                || intent.digest().map_err(|_| JournalError::Format)? != payload_digest
            {
                return Err(JournalError::Format);
            }
            Ok(ReceivedNodeIntent {
                intent,
                payload_digest,
                receipt_id,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orange_protocol::node_control::StopKind;
    fn directory() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private");
        }
        dir
    }
    fn identity(epoch: u64) -> ControlIdentity {
        ControlIdentity {
            node_id: "node".into(),
            session_id: format!("session-{epoch}"),
            connection_epoch: epoch,
        }
    }
    fn intent(generation: &str) -> NodeStopIntent {
        NodeStopIntent {
            command_id: format!("command-{generation}"),
            action_id: format!("action-{generation}"),
            node_id: "node".into(),
            policy_generation: generation.into(),
            kind: StopKind::Drain,
        }
    }
    #[test]
    fn durable_receipt_survives_restart_and_rejects_old_session_and_generation() {
        let dir = directory();
        let path = dir.path().join("commands.sqlite");
        let mut journal = NodeJournal::open(&path, "node".into()).expect("open");
        assert!(
            NodeJournal::open(&path, "node".into()).is_err(),
            "exclusive owner"
        );
        journal
            .bind(&identity(1))
            .expect("explicit transport fixture");
        let command = intent("9007199254740993");
        let digest = command.digest().expect("digest");
        let first = journal
            .receive(&identity(1), &command, &digest)
            .expect("persist");
        assert_eq!(
            first,
            journal
                .receive(&identity(1), &command, &digest)
                .expect("duplicate")
        );
        drop(journal);
        assert!(NodeJournal::open(&path, "other-node".into()).is_err());
        let mut journal = NodeJournal::open(&path, "node".into()).expect("restart");
        assert_eq!(
            journal.received(&command.command_id).expect("read"),
            Some(first.clone())
        );
        journal.bind(&identity(2)).expect("current fixture");
        assert!(matches!(
            journal.receive(&identity(1), &command, &digest),
            Err(JournalError::Stale)
        ));
        assert!(matches!(
            journal.bind(&identity(1)),
            Err(JournalError::Stale)
        ));
        assert_eq!(
            first,
            journal
                .receive(&identity(2), &command, &digest)
                .expect("only receipt replay, no execution")
        );
        let newer = intent("9007199254740994");
        journal
            .receive(&identity(2), &newer, &newer.digest().expect("digest"))
            .expect("newer intent");
        assert!(matches!(
            journal.receive(&identity(2), &command, &digest),
            Err(JournalError::Stale)
        ));
    }
    #[test]
    fn body_conflict_wrong_scope_and_disk_fault_cannot_emit_receipt() {
        let dir = directory();
        let mut journal =
            NodeJournal::open(&dir.path().join("commands.sqlite"), "node".into()).expect("open");
        journal.bind(&identity(1)).expect("fixture binding");
        let command = intent("1");
        journal
            .receive(&identity(1), &command, &command.digest().expect("digest"))
            .expect("first");
        let mut changed = command.clone();
        changed.kind = StopKind::Quarantine;
        assert!(matches!(
            journal.receive(&identity(1), &changed, &changed.digest().expect("digest")),
            Err(JournalError::Conflict)
        ));
        changed = intent("2");
        changed.node_id = "other-node".into();
        assert!(matches!(
            journal.receive(&identity(1), &changed, &changed.digest().expect("digest")),
            Err(JournalError::Invalid)
        ));
        assert!(
            journal
                .connection
                .execute("DELETE FROM node_intents", [])
                .is_err()
        );
        journal.connection.execute_batch("CREATE TEMP TRIGGER disk_fault BEFORE INSERT ON node_intents BEGIN SELECT RAISE(ABORT,'test write failure'); END;").expect("fault");
        let newer = intent("2");
        assert!(
            journal
                .receive(&identity(1), &newer, &newer.digest().expect("digest"))
                .is_err()
        );
        assert!(
            journal
                .received(&newer.command_id)
                .expect("no record")
                .is_none()
        );
        let generation: i64 = journal
            .connection
            .query_row("SELECT policy_generation FROM node_receiver", [], |r| {
                r.get(0)
            })
            .expect("unchanged generation");
        assert_eq!(generation, 1);
    }
    #[test]
    fn future_journal_format_is_not_reinterpreted() {
        let dir = directory();
        let path = dir.path().join("commands.sqlite");
        let journal = NodeJournal::open(&path, "node".into()).expect("open");
        journal
            .connection
            .execute_batch("PRAGMA user_version=999;")
            .expect("future fixture");
        drop(journal);
        assert!(matches!(
            NodeJournal::open(&path, "node".into()),
            Err(JournalError::Format)
        ));
    }
}
