//! Durable E08 command identity. Public inspection is not execution authority.
//! Only the authenticated in-crate control adapter can mutate this journal.
#[cfg(unix)]
mod cpu;
mod rejections;
use crate::{
    broker::{BrokerError, Grant, Holder, storage},
    control::ControlIdentity,
};
use orange_protocol::{
    execution::{acknowledge_digest, assignment_digest},
    grpc::worker,
    valid_id,
};
use prost::Message;
pub use rejections::{RejectionKind, RejectionView};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{fs::File, path::Path};
#[derive(Debug, thiserror::Error)]
pub enum ExecutionJournalError {
    #[error("invalid or conflicting execution command")]
    Invalid,
    #[error("stale session or uncertain execution requires reconciliation")]
    Stale,
    #[error("unsupported execution journal format")]
    Format,
    #[error(transparent)]
    Storage(#[from] BrokerError),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}
#[derive(Clone, Debug)]
pub struct ExecutionRecord {
    pub command_id: String,
    pub journal_id: String,
    pub state: String,
    pub grant: Option<Grant>,
    pub rejection: Option<RejectionView>,
    pub(crate) assignment: worker::Assignment,
}
pub struct ExecutionJournal {
    connection: Connection,
    _lock: File,
    node: String,
}
impl ExecutionJournal {
    pub fn open(path: &Path, node: String) -> Result<Self, ExecutionJournalError> {
        if !valid_id(&node)
            || !node
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(ExecutionJournalError::Invalid);
        }
        let (mut connection, lock) = storage::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(0..=3).contains(&version) {
            return Err(ExecutionJournalError::Format);
        }
        if version == 0 {
            let count:i64=connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
            if count != 0 {
                return Err(ExecutionJournalError::Format);
            }
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=100;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            tx.execute_batch(include_str!("execution_schema.sql"))?;
            tx.execute("INSERT INTO execution_receiver(singleton,node_id,session_id,connection_epoch) VALUES(1,?,'',0)",[&node])?;
        } else {
            let old: String = tx.query_row(
                "SELECT node_id FROM execution_receiver WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            if old != node {
                return Err(ExecutionJournalError::Invalid);
            }
            // No wall-clock reconstruction of a previous process's monotonic
            // anchors, no effect replay even if a StartGrant had been recorded.
            tx.execute("UPDATE execution_commands SET state='unknown' WHERE state NOT IN ('unknown','rejected')",[])?;
        }
        if version < 2 {
            tx.execute_batch(include_str!("execution_schema_v2.sql"))?;
        }
        if version < 3 {
            tx.execute_batch(include_str!("execution_schema_v3.sql"))?;
        }
        tx.execute("UPDATE cpu_runs SET phase='cleanup_unknown',error_code='OWNER_RESTARTED' WHERE phase IN ('starting','generating')",[])?;
        tx.commit()?;
        Ok(Self {
            connection,
            _lock: lock,
            node,
        })
    }
    pub fn node_id(&self) -> &str {
        &self.node
    }
    pub fn record(&self, command: &str) -> Result<Option<ExecutionRecord>, ExecutionJournalError> {
        if !valid_id(command) {
            return Err(ExecutionJournalError::Invalid);
        }
        let row:Option<(String,String,Vec<u8>,Option<String>)>=self.connection.query_row("SELECT journal_id,state,assignment,grant_json FROM execution_commands WHERE command_id=?",[command],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        row.map(|(journal_id, state, bytes, grant)| {
            let a = worker::Assignment::decode(bytes.as_slice())
                .map_err(|_| ExecutionJournalError::Format)?;
            if assignment_digest(&a).as_deref() != Some(a.payload_digest.as_str())
                || a.command_id != command
                || a.node_id != self.node
            {
                return Err(ExecutionJournalError::Format);
            }
            let grant: Option<Grant> = grant
                .map(|s| serde_json::from_str(&s).map_err(|_| ExecutionJournalError::Format))
                .transpose()?;
            let rejection = self.rejection_view(command, &a)?;
            if (state == "rejected") != rejection.is_some()
                || (rejection.is_some() && grant.is_some())
            {
                return Err(ExecutionJournalError::Format);
            }
            Ok(ExecutionRecord {
                command_id: command.into(),
                journal_id,
                state,
                grant,
                rejection,
                assignment: a,
            })
        })
        .transpose()
    }
    pub(crate) fn bind(&mut self, identity: &ControlIdentity) -> Result<(), ExecutionJournalError> {
        if identity.node_id != self.node
            || !valid_id(&identity.session_id)
            || identity.connection_epoch == 0
            || identity.connection_epoch > i64::MAX as u64
        {
            return Err(ExecutionJournalError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (old, session): (i64, String) = tx.query_row(
            "SELECT connection_epoch,session_id FROM execution_receiver WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if old > identity.connection_epoch as i64
            || (old == identity.connection_epoch as i64 && session != identity.session_id)
        {
            return Err(ExecutionJournalError::Stale);
        }
        if old != identity.connection_epoch as i64 {
            tx.execute("UPDATE execution_commands SET state='unknown' WHERE state NOT IN ('unknown','rejected')",[])?;
        }
        tx.execute(
            "UPDATE execution_receiver SET session_id=?,connection_epoch=? WHERE singleton=1",
            params![identity.session_id, identity.connection_epoch as i64],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn current(&self, identity: &ControlIdentity) -> Result<(), ExecutionJournalError> {
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM execution_receiver WHERE singleton=1 AND node_id=? AND session_id=? AND connection_epoch=?)",params![identity.node_id,identity.session_id,i64::try_from(identity.connection_epoch).map_err(|_|ExecutionJournalError::Invalid)?],|r|r.get(0))?;
        if !valid {
            return Err(ExecutionJournalError::Stale);
        }
        Ok(())
    }
    pub(crate) fn receive(
        &mut self,
        identity: &ControlIdentity,
        a: &worker::Assignment,
    ) -> Result<ExecutionRecord, ExecutionJournalError> {
        self.current(identity)?;
        if a.node_id != identity.node_id
            || a.session_id != identity.session_id
            || a.lease
                .as_ref()
                .is_none_or(|l| l.connection_epoch != identity.connection_epoch)
            || assignment_digest(a).as_deref() != Some(a.payload_digest.as_str())
        {
            return Err(ExecutionJournalError::Invalid);
        }
        if let Some(record) = self.record(&a.command_id)? {
            if record.assignment != *a {
                return Err(ExecutionJournalError::Invalid);
            }
            return Ok(record);
        }
        let journal_id = uuid::Uuid::new_v4().to_string();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO execution_commands(command_id,attempt_id,journal_id,assignment,state) VALUES(?,?,?,?,'received')",params![a.command_id,a.lease.as_ref().ok_or(ExecutionJournalError::Invalid)?.attempt_id,journal_id,a.encode_to_vec()])?;
        tx.commit()?;
        self.record(&a.command_id)?
            .ok_or(ExecutionJournalError::Format)
    }
    pub(crate) fn reserve(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        grant: &Grant,
    ) -> Result<(), ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.assignment.session_id != identity.session_id
            || record.state != "received"
            || grant.mode != Holder::Remote
            || grant.profile_id != record.assignment.execution_profile_id
            || grant.broker_epoch <= 0
            || grant.slot_generation <= 0
        {
            return Err(ExecutionJournalError::Stale);
        }
        let json = serde_json::to_string(grant).map_err(|_| ExecutionJournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE execution_commands SET grant_json=?,state='reserved' WHERE command_id=?",
            params![json, command],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn send_ack(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
    ) -> Result<worker::Acknowledge, ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.state != "reserved" || record.assignment.session_id != identity.session_id {
            return Err(ExecutionJournalError::Stale);
        }
        let ack = record.ack()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE execution_commands SET state='ack_sent' WHERE command_id=?",
            [command],
        )?;
        tx.commit()?;
        Ok(ack)
    }
    pub(crate) fn claim_start(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        grant: &worker::StartGrant,
    ) -> Result<Grant, ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        let ack = record.ack()?;
        if record.state != "ack_sent"
            || record.assignment.session_id != identity.session_id
            || grant.command_id != ack.command_id
            || grant.lease != ack.lease
            || grant.payload_digest != ack.payload_digest
            || grant.execution_journal_id != ack.execution_journal_id
            || grant.broker_grant_id != ack.broker_grant_id
            || grant.broker_epoch != ack.broker_epoch
            || grant.slot_generation != ack.slot_generation
            || !(1001..=45000).contains(&grant.start_budget_ms)
        {
            return Err(ExecutionJournalError::Stale);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE execution_commands SET state='start_claimed' WHERE command_id=?",
            [command],
        )?;
        tx.commit()?;
        record.grant.ok_or(ExecutionJournalError::Format)
    }
    pub(crate) fn running(&mut self, command: &str) -> Result<(), ExecutionJournalError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE execution_commands SET state='running' WHERE command_id=? AND state='start_claimed'",[command])?!=1 {return Err(ExecutionJournalError::Stale);}
        tx.commit()?;
        Ok(())
    }
}
impl ExecutionRecord {
    pub(crate) fn ack(&self) -> Result<worker::Acknowledge, ExecutionJournalError> {
        let grant = self.grant.as_ref().ok_or(ExecutionJournalError::Stale)?;
        let a = worker::Acknowledge {
            command_id: self.command_id.clone(),
            payload_digest: self.assignment.payload_digest.clone(),
            lease: self.assignment.lease.clone(),
            result: worker::AckResult::Accepted as i32,
            execution_journal_id: self.journal_id.clone(),
            broker_grant_id: grant.grant_id.clone(),
            broker_epoch: grant.broker_epoch as u64,
            slot_generation: grant.slot_generation as u64,
        };
        acknowledge_digest(&a, &self.assignment).ok_or(ExecutionJournalError::Invalid)?;
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(epoch: u64) -> ControlIdentity {
        ControlIdentity {
            node_id: "node".into(),
            session_id: format!("session-{epoch}"),
            connection_epoch: epoch,
        }
    }
    fn assignment() -> worker::Assignment {
        let mut a = worker::Assignment {
            command_id: "command".into(),
            payload_digest: String::new(),
            node_id: "node".into(),
            session_id: "session-1".into(),
            node_policy_generation: 0,
            model_version_id: "model".into(),
            model_sha256: "a".repeat(64),
            execution_profile_id: "profile".into(),
            execution_profile_digest: "b".repeat(64),
            request_projection_digest: "c".repeat(64),
            input_artifact_ids: vec![],
            input_artifacts: vec![],
            lease: Some(worker::Lease {
                lease_id: "lease".into(),
                attempt_id: "attempt".into(),
                job_id: "job".into(),
                connection_epoch: 1,
                fencing_token: 1,
                expires_at: Some(prost_types_fixture(2_000_000_045)),
            }),
            job_deadline: Some(prost_types_fixture(2_000_000_100)),
        };
        a.payload_digest = assignment_digest(&a).expect("digest");
        a
    }
    // Timestamp type comes from the wire field; no independent clock authority.
    fn prost_types_fixture(seconds: i64) -> prost_types::Timestamp {
        prost_types::Timestamp { seconds, nanos: 0 }
    }
    fn grant() -> Grant {
        Grant {
            grant_id: "grant".into(),
            device_id: "cpu".into(),
            broker_epoch: 1,
            slot_generation: 1,
            supervisor_instance: "supervisor".into(),
            mode: Holder::Remote,
            profile_id: "profile".into(),
        }
    }
    fn private() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("temp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private");
        }
        d
    }
    fn start(a: &worker::Acknowledge) -> worker::StartGrant {
        worker::StartGrant {
            command_id: a.command_id.clone(),
            lease: a.lease.clone(),
            payload_digest: a.payload_digest.clone(),
            execution_journal_id: a.execution_journal_id.clone(),
            broker_grant_id: a.broker_grant_id.clone(),
            broker_epoch: a.broker_epoch,
            slot_generation: a.slot_generation,
            start_budget_ms: 40000,
        }
    }
    #[test]
    fn durable_assignment_ack_and_exact_start_cannot_replay_after_restart() {
        let d = private();
        let path = d.path().join("execution.sqlite");
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("open");
        assert!(ExecutionJournal::open(&path, "node".into()).is_err());
        let a = assignment();
        assert!(j.receive(&identity(1), &a).is_err());
        j.bind(&identity(1)).expect("authenticated fixture");
        let first = j.receive(&identity(1), &a).expect("receive");
        assert_eq!(
            j.receive(&identity(1), &a).expect("duplicate").journal_id,
            first.journal_id
        );
        assert!(
            j.send_ack(&identity(1), &a.command_id).is_err(),
            "no ACK before local grant"
        );
        j.reserve(&identity(1), &a.command_id, &grant())
            .expect("real grant fixture");
        let ack = j
            .send_ack(&identity(1), &a.command_id)
            .expect("durable ACK");
        assert!(
            j.send_ack(&identity(1), &a.command_id).is_err(),
            "cannot reset send anchor"
        );
        let mut wrong = start(&ack);
        wrong.slot_generation += 1;
        assert!(j.claim_start(&identity(1), &a.command_id, &wrong).is_err());
        let started = j
            .claim_start(&identity(1), &a.command_id, &start(&ack))
            .expect("claim");
        assert_eq!(started, grant());
        assert!(
            j.claim_start(&identity(1), &a.command_id, &start(&ack))
                .is_err()
        );
        j.running(&a.command_id).expect("record");
        drop(j);
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("restart");
        let record = j.record(&a.command_id).expect("read").expect("record");
        assert_eq!(record.state, "unknown");
        assert_eq!(record.journal_id, first.journal_id);
        assert!(j.send_ack(&identity(1), &a.command_id).is_err());
        assert!(
            j.claim_start(&identity(1), &a.command_id, &start(&ack))
                .is_err()
        );
    }
    #[test]
    fn commit_failure_never_produces_ack_and_future_formats_are_refused() {
        let d = private();
        let path = d.path().join("execution.sqlite");
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("open");
        j.bind(&identity(1)).expect("bind");
        let a = assignment();
        j.receive(&identity(1), &a).expect("receive");
        j.reserve(&identity(1), &a.command_id, &grant())
            .expect("reserve");
        j.connection.execute_batch("CREATE TEMP TRIGGER write_fault BEFORE UPDATE ON execution_commands WHEN NEW.state='ack_sent' BEGIN SELECT RAISE(ABORT,'explicit disk failure fixture'); END;").expect("fault");
        assert!(j.send_ack(&identity(1), &a.command_id).is_err());
        assert_eq!(
            j.record(&a.command_id)
                .expect("read")
                .expect("record")
                .state,
            "reserved"
        );
        assert!(
            j.connection
                .execute("DELETE FROM execution_commands", [])
                .is_err()
        );
        j.connection
            .execute_batch("PRAGMA user_version=999;")
            .expect("future fixture");
        drop(j);
        assert!(matches!(
            ExecutionJournal::open(&path, "node".into()),
            Err(ExecutionJournalError::Format)
        ));
    }
    #[test]
    fn conflicting_payload_and_new_connection_cannot_inherit_old_permission() {
        let d = private();
        let mut j = ExecutionJournal::open(&d.path().join("execution.sqlite"), "node".into())
            .expect("open");
        j.bind(&identity(1)).expect("bind");
        let a = assignment();
        j.receive(&identity(1), &a).expect("receive");
        let mut changed = a.clone();
        changed.execution_profile_id = "other".into();
        changed.payload_digest = assignment_digest(&changed).expect("digest");
        assert!(j.receive(&identity(1), &changed).is_err());
        j.bind(&identity(2)).expect("new session");
        assert!(j.bind(&identity(1)).is_err());
        assert!(j.reserve(&identity(1), &a.command_id, &grant()).is_err());
        assert!(j.receive(&identity(2), &a).is_err());
        assert_eq!(
            j.record(&a.command_id)
                .expect("read")
                .expect("record")
                .state,
            "unknown"
        );
    }
    #[test]
    fn negative_decision_and_cloud_receipt_survive_restart_without_start_permission() {
        let d = private();
        let path = d.path().join("execution.sqlite");
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("open");
        j.bind(&identity(1)).expect("bind");
        let a = assignment();
        j.receive(&identity(1), &a).expect("receive");
        let view = j
            .reject(&identity(1), &a.command_id, RejectionKind::Resource)
            .expect("persist negative");
        assert!(view.receipt_id.is_none());
        assert!(j.reserve(&identity(1), &a.command_id, &grant()).is_err());
        assert!(j.send_ack(&identity(1), &a.command_id).is_err());
        let ack = j
            .rejected_ack(&identity(1), &a.command_id)
            .expect("original negative");
        assert_eq!(ack.result, worker::AckResult::ResourceRejected as i32);
        assert!(ack.broker_grant_id.is_empty());
        drop(j);
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("restart");
        j.bind(&identity(2)).expect("new current transport fixture");
        assert_eq!(
            j.rejected_ack(&identity(2), &a.command_id)
                .expect("old command evidence"),
            ack
        );
        let receipt = worker::RejectionReceipt {
            command_id: ack.command_id.clone(),
            payload_digest: ack.payload_digest.clone(),
            lease: ack.lease.clone(),
            result: ack.result,
            receipt_id: "server-receipt-fixture".into(),
        };
        let mut wrong = receipt.clone();
        wrong.result = worker::AckResult::ProtocolRejected as i32;
        assert!(
            j.confirm_rejection(&identity(2), &a.command_id, &wrong)
                .is_err()
        );
        let confirmed = j
            .confirm_rejection(&identity(2), &a.command_id, &receipt)
            .expect("exact receipt");
        assert_eq!(
            confirmed.receipt_id.as_deref(),
            Some("server-receipt-fixture")
        );
        assert_eq!(
            j.confirm_rejection(&identity(2), &a.command_id, &receipt)
                .expect("idempotent receipt"),
            confirmed
        );
        wrong = receipt;
        wrong.receipt_id = "different-receipt".into();
        assert!(
            j.confirm_rejection(&identity(2), &a.command_id, &wrong)
                .is_err()
        );
        assert!(
            j.connection
                .execute("DELETE FROM execution_rejections", [])
                .is_err()
        );
        assert!(j.send_ack(&identity(2), &a.command_id).is_err());
    }
    #[test]
    fn negative_write_failure_keeps_command_undecided_and_cannot_emit_ack() {
        let d = private();
        let mut j = ExecutionJournal::open(&d.path().join("execution.sqlite"), "node".into())
            .expect("open");
        j.bind(&identity(1)).expect("bind");
        let a = assignment();
        j.receive(&identity(1), &a).expect("receive");
        j.connection.execute_batch("CREATE TEMP TRIGGER rejection_fault BEFORE INSERT ON execution_rejections BEGIN SELECT RAISE(ABORT,'explicit disk fault'); END;").expect("fault");
        assert!(
            j.reject(&identity(1), &a.command_id, RejectionKind::Protocol)
                .is_err()
        );
        assert!(j.rejected_ack(&identity(1), &a.command_id).is_err());
        assert_eq!(
            j.record(&a.command_id)
                .expect("read")
                .expect("record")
                .state,
            "received"
        );
        j.connection
            .execute_batch("DROP TRIGGER rejection_fault;")
            .expect("repair fixture");
        j.reserve(&identity(1), &a.command_id, &grant())
            .expect("positive reservation fixture");
        assert!(
            j.reject(&identity(1), &a.command_id, RejectionKind::Resource)
                .is_err(),
            "cannot change a potentially reserved command into ordinary rejection"
        );
    }
    #[test]
    fn journal_v1_upgrade_retains_prior_grant_and_never_invents_rejection() {
        let d = private();
        let path = d.path().join("execution.sqlite");
        let (connection, lock) = storage::open(&path).expect("old private file");
        connection
            .execute_batch(include_str!("execution_schema.sql"))
            .expect("actual committed v1 schema");
        connection.execute("INSERT INTO execution_receiver(singleton,node_id,session_id,connection_epoch) VALUES(1,'node','session-1',1)",[]).expect("meta");
        let a = assignment();
        connection.execute("INSERT INTO execution_commands(command_id,attempt_id,journal_id,assignment,grant_json,state) VALUES('command','attempt','original-journal',?,?,'reserved')",params![a.encode_to_vec(),serde_json::to_string(&grant()).expect("grant")]).expect("history");
        drop(connection);
        drop(lock);
        let j = ExecutionJournal::open(&path, "node".into()).expect("upgrade");
        let record = j.record("command").expect("read").expect("history");
        assert_eq!(record.state, "unknown");
        assert_eq!(record.grant, Some(grant()));
        assert_eq!(record.journal_id, "original-journal");
        assert!(record.rejection.is_none());
        assert_eq!(
            j.connection
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .expect("version"),
            3
        );
    }
    #[cfg(unix)]
    #[test]
    fn cpu_effect_is_unique_and_restart_preserves_unknown_cleanup() {
        let d = private();
        let root = private();
        let other_root = private();
        let path = d.path().join("execution.sqlite");
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("open");
        j.bind_cpu_root(root.path()).expect("first binding");
        j.bind_cpu_root(root.path()).expect("same physical root");
        assert!(j.bind_cpu_root(other_root.path()).is_err());
        assert!(
            j.connection
                .execute("DELETE FROM cpu_runtime_binding", [])
                .is_err()
        );
        j.bind(&identity(1)).expect("identity fixture");
        let a = assignment();
        j.receive(&identity(1), &a).expect("receive");
        assert!(
            j.begin_cpu(
                &identity(1),
                "command",
                "instance",
                &a.request_projection_digest,
                &a.execution_profile_digest
            )
            .is_err()
        );
        j.reserve(&identity(1), "command", &grant())
            .expect("reserve fixture");
        let ack = j.send_ack(&identity(1), "command").expect("ack");
        j.claim_start(&identity(1), "command", &start(&ack))
            .expect("claim");
        j.running("command").expect("running");
        j.begin_cpu(
            &identity(1),
            "command",
            "instance",
            &a.request_projection_digest,
            &a.execution_profile_digest,
        )
        .expect("before effect");
        assert!(
            j.begin_cpu(
                &identity(1),
                "command",
                "other",
                &a.request_projection_digest,
                &a.execution_profile_digest
            )
            .is_err()
        );
        j.cpu_phase("command", "generating").expect("generating");
        drop(j);
        let mut j = ExecutionJournal::open(&path, "node".into()).expect("restart");
        let state: (String, String, bool) = j.connection.query_row("SELECT phase,error_code,cleanup_confirmed FROM cpu_runs WHERE command_id='command'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).expect("persisted uncertainty");
        assert_eq!(
            state,
            ("cleanup_unknown".into(), "OWNER_RESTARTED".into(), false)
        );
        assert!(
            j.finish_cpu(
                "command",
                "computed_unsubmitted",
                Some(&"d".repeat(64)),
                Some(&"e".repeat(64)),
                None
            )
            .is_err()
        );
        assert!(j.connection.execute("DELETE FROM cpu_runs", []).is_err());
        assert!(j.bind_cpu_root(other_root.path()).is_err());
        j.bind_cpu_root(root.path())
            .expect("original root retained");
    }
}
