//! Durable local operation boundary. Not yet a model loop or an OS sandbox.
pub mod workspace;
use orange_domain::{ExecutionIdentity, validate_response};
use orange_protocol::{valid_digest, valid_id};
use rusqlite::{Connection, OptionalExtension, params};
use std::{fs::File, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("invalid command or identity")]
    Invalid,
    #[error("another host owns this journal")]
    Busy,
    #[error("operation identity/content conflict")]
    Conflict,
    #[error("response is not durably committed")]
    ResponseRequired,
    #[error("run or policy was revoked")]
    Revoked,
    #[error("approval is missing, stale or expired")]
    ApprovalRequired,
    #[error("operation has already dispatched; reconcile before any retry")]
    AlreadyDispatched,
    #[error("invalid operation transition")]
    Transition,
    #[error("unsupported journal format; refusing to alter existing data")]
    UnsupportedVersion,
    #[error("journal storage failure")]
    Storage(#[from] rusqlite::Error),
    #[error("journal filesystem failure")]
    Io(#[from] std::io::Error),
}

pub struct Journal {
    connection: Connection,
    _host_lock: File,
}
pub struct ResponseAuthority {
    pub current: ExecutionIdentity,
    pub incoming: ExecutionIdentity,
    pub complete: bool,
}
pub struct Operation<'a> {
    pub id: &'a str,
    pub owner: &'a str,
    pub run: &'a str,
    pub invocation: &'a str,
    pub argument_digest: &'a str,
    pub tool_version: &'a str,
    pub environment: &'a str,
    pub policy_version: &'a str,
    pub requires_approval: bool,
}

impl Journal {
    /// Caller must supply its private application data directory, not an untrusted workspace path.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let parent = path.parent().ok_or(JournalError::Invalid)?.canonicalize()?;
        let normalized = if path.exists() {
            path.canonicalize()?
        } else {
            parent.join(path.file_name().ok_or(JournalError::Invalid)?)
        };
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(normalized.with_extension("host-lock"))?;
        lock.try_lock().map_err(|_| JournalError::Busy)?;
        let connection = Connection::open(normalized)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(0..=1).contains(&version) {
            return Err(JournalError::UnsupportedVersion);
        }
        if version == 0 {
            let existing:i64=connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
            if existing != 0 {
                return Err(JournalError::UnsupportedVersion);
            }
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS runs(run_id TEXT PRIMARY KEY,owner TEXT NOT NULL,policy TEXT NOT NULL,active INTEGER NOT NULL CHECK(active IN(0,1)));
            CREATE TABLE IF NOT EXISTS responses(invocation TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES runs(run_id),attempt INTEGER NOT NULL,digest TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS operations(operation_id TEXT PRIMARY KEY,owner TEXT NOT NULL,run_id TEXT NOT NULL REFERENCES runs(run_id),invocation TEXT NOT NULL REFERENCES responses(invocation),argument_digest TEXT NOT NULL,tool_version TEXT NOT NULL,environment TEXT NOT NULL,policy TEXT NOT NULL,requires_approval INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN('intended','dispatching','succeeded','unknown')),result_digest TEXT);
            CREATE TABLE IF NOT EXISTS approvals(operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),actor TEXT NOT NULL,argument_digest TEXT NOT NULL,environment TEXT NOT NULL,policy TEXT NOT NULL,expires_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,operation_id TEXT,kind TEXT NOT NULL);
            INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'operation.unknown' FROM operations WHERE state='dispatching';
            UPDATE operations SET state='unknown' WHERE state='dispatching';
            PRAGMA user_version=1;
            COMMIT;")?;
        Ok(Self {
            connection,
            _host_lock: lock,
        })
    }
    pub fn register_run(&self, owner: &str, run: &str, policy: &str) -> Result<(), JournalError> {
        if ![owner, run, policy].iter().all(|s| valid_id(s)) {
            return Err(JournalError::Invalid);
        }
        let old: Option<(String, String)> = self
            .connection
            .query_row("SELECT owner,policy FROM runs WHERE run_id=?", [run], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if let Some(pair) = old {
            if pair != (owner.into(), policy.into()) {
                return Err(JournalError::Conflict);
            }
            return Ok(());
        }
        self.connection.execute(
            "INSERT INTO runs(run_id,owner,policy,active) VALUES(?,?,?,1)",
            params![run, owner, policy],
        )?;
        Ok(())
    }
    pub fn revoke_run(&self, owner: &str, run: &str) -> Result<(), JournalError> {
        if self.connection.execute(
            "UPDATE runs SET active=0 WHERE run_id=? AND owner=?",
            params![run, owner],
        )? != 1
        {
            return Err(JournalError::Revoked);
        }
        Ok(())
    }
    /// Invoked only by the trusted model adapter after validating complete response/authority.
    pub fn commit_response(
        &self,
        owner: &str,
        run: &str,
        invocation: &str,
        authority: ResponseAuthority,
        digest: &str,
    ) -> Result<(), JournalError> {
        let ResponseAuthority {
            current,
            incoming,
            complete,
        } = authority;
        if !valid_id(invocation) || !valid_digest(digest) {
            return Err(JournalError::Invalid);
        }
        let active: bool = self
            .connection
            .query_row(
                "SELECT active FROM runs WHERE run_id=? AND owner=?",
                params![run, owner],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        validate_response(current, incoming, complete, active)
            .map_err(|_| JournalError::ResponseRequired)?;
        let old: Option<(String, u8, String)> = self
            .connection
            .query_row(
                "SELECT run_id,attempt,digest FROM responses WHERE invocation=?",
                [invocation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some(value) = old {
            if value != (run.into(), incoming.attempt, digest.into()) {
                return Err(JournalError::Conflict);
            }
            return Ok(());
        }
        self.connection.execute(
            "INSERT INTO responses(invocation,run_id,attempt,digest) VALUES(?,?,?,?)",
            params![invocation, run, incoming.attempt, digest],
        )?;
        Ok(())
    }
    pub fn prepare(&mut self, op: &Operation<'_>) -> Result<(), JournalError> {
        if ![
            op.id,
            op.owner,
            op.run,
            op.invocation,
            op.tool_version,
            op.environment,
            op.policy_version,
        ]
        .iter()
        .all(|s| valid_id(s))
            || !valid_digest(op.argument_digest)
        {
            return Err(JournalError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE run_id=? AND owner=? AND policy=? AND active=1)",params![op.run,op.owner,op.policy_version],|r|r.get(0))?;
        if !valid {
            return Err(JournalError::Revoked);
        }
        let response: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM responses WHERE invocation=? AND run_id=?)",
            params![op.invocation, op.run],
            |r| r.get(0),
        )?;
        if !response {
            return Err(JournalError::ResponseRequired);
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=?)",
            [op.id],
            |r| r.get(0),
        )?;
        if exists {
            let same:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=? AND owner=? AND run_id=? AND invocation=? AND argument_digest=? AND tool_version=? AND environment=? AND policy=? AND requires_approval=?)",params![op.id,op.owner,op.run,op.invocation,op.argument_digest,op.tool_version,op.environment,op.policy_version,op.requires_approval],|r|r.get(0))?;
            if !same {
                return Err(JournalError::Conflict);
            }
            tx.commit()?;
            return Ok(());
        }
        tx.execute("INSERT INTO operations(operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state) VALUES(?,?,?,?,?,?,?,?,?,'intended')",params![op.id,op.owner,op.run,op.invocation,op.argument_digest,op.tool_version,op.environment,op.policy_version,op.requires_approval])?;
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.intended')",
            params![op.run, op.id],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// The approval actor is obtained from the trusted interactive/authenticated layer.
    pub fn approve(
        &self,
        op: &Operation<'_>,
        actor: &str,
        expires_at: i64,
    ) -> Result<(), JournalError> {
        if !valid_id(actor) {
            return Err(JournalError::Invalid);
        }
        let exists:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=? AND owner=? AND run_id=? AND argument_digest=? AND environment=? AND policy=? AND state='intended')",params![op.id,op.owner,op.run,op.argument_digest,op.environment,op.policy_version],|r|r.get(0))?;
        if !exists {
            return Err(JournalError::ApprovalRequired);
        }
        self.connection.execute("INSERT INTO approvals(operation_id,actor,argument_digest,environment,policy,expires_at) VALUES(?,?,?,?,?,?) ON CONFLICT(operation_id) DO UPDATE SET actor=excluded.actor,argument_digest=excluded.argument_digest,environment=excluded.environment,policy=excluded.policy,expires_at=excluded.expires_at",params![op.id,actor,op.argument_digest,op.environment,op.policy_version,expires_at])?;
        Ok(())
    }
    /// Returns only after the dispatch barrier is durable. Unknown is never auto-retried.
    pub fn claim(&mut self, op: &Operation<'_>, now: i64) -> Result<(), JournalError> {
        self.prepare(op)?;
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if op.requires_approval {
            let approved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM approvals WHERE operation_id=? AND argument_digest=? AND environment=? AND policy=? AND expires_at>?)",params![op.id,op.argument_digest,op.environment,op.policy_version,now],|r|r.get(0))?;
            if !approved {
                return Err(JournalError::ApprovalRequired);
            }
        }
        if tx.execute(
            "UPDATE operations SET state='dispatching' WHERE operation_id=? AND state='intended'",
            [op.id],
        )? != 1
        {
            return Err(JournalError::AlreadyDispatched);
        }
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.dispatching')",
            params![op.run, op.id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn complete(
        &mut self,
        owner: &str,
        id: &str,
        result_digest: &str,
    ) -> Result<(), JournalError> {
        if !valid_digest(result_digest) {
            return Err(JournalError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE operations SET state='succeeded',result_digest=? WHERE operation_id=? AND owner=? AND state='dispatching'",params![result_digest,id,owner])?!=1{return Err(JournalError::Transition);}
        tx.execute("INSERT INTO events(run_id,operation_id,kind) SELECT run_id,operation_id,'operation.succeeded' FROM operations WHERE operation_id=?",[id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn state(&self, owner: &str, id: &str) -> Result<Option<String>, JournalError> {
        Ok(self
            .connection
            .query_row(
                "SELECT state FROM operations WHERE operation_id=? AND owner=?",
                params![id, owner],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> ExecutionIdentity {
        ExecutionIdentity {
            connection_epoch: 1,
            fencing: 1,
            attempt: 1,
        }
    }
    fn operation(digest: &str) -> Operation<'_> {
        Operation {
            id: "op",
            owner: "alice",
            run: "run",
            invocation: "inv",
            argument_digest: digest,
            tool_version: "tool-v1",
            environment: "workspace",
            policy_version: "policy-v1",
            requires_approval: true,
        }
    }
    #[test]
    fn no_tool_dispatch_without_valid_response_approval_and_current_run() {
        let dir = tempfile::tempdir().expect("temp");
        let mut j = Journal::open(&dir.path().join("state.sqlite")).expect("open");
        j.register_run("alice", "run", "policy-v1").expect("run");
        let hash = "a".repeat(64);
        let op = operation(&hash);
        assert!(matches!(
            j.prepare(&op),
            Err(JournalError::ResponseRequired)
        ));
        assert!(
            j.commit_response(
                "alice",
                "run",
                "inv",
                ResponseAuthority {
                    current: identity(),
                    incoming: identity(),
                    complete: false
                },
                &hash
            )
            .is_err()
        );
        j.commit_response(
            "alice",
            "run",
            "inv",
            ResponseAuthority {
                current: identity(),
                incoming: identity(),
                complete: true,
            },
            &hash,
        )
        .expect("response");
        j.prepare(&op).expect("intent");
        assert!(j.claim(&op, 10).is_err());
        j.approve(&op, "alice", 20).expect("approval");
        assert!(j.claim(&op, 21).is_err());
        j.approve(&op, "alice", 30).expect("approval");
        j.revoke_run("alice", "run").expect("revoke");
        assert!(matches!(j.claim(&op, 22), Err(JournalError::Revoked)));
    }
    #[test]
    fn crash_after_dispatch_is_unknown_and_never_replayed() {
        let dir = tempfile::tempdir().expect("temp");
        let file = dir.path().join("state.sqlite");
        let hash = "a".repeat(64);
        let op = operation(&hash);
        {
            let mut j = Journal::open(&file).expect("open");
            j.register_run("alice", "run", "policy-v1").expect("run");
            j.commit_response(
                "alice",
                "run",
                "inv",
                ResponseAuthority {
                    current: identity(),
                    incoming: identity(),
                    complete: true,
                },
                &hash,
            )
            .expect("response");
            j.prepare(&op).expect("intent");
            j.approve(&op, "alice", 100).expect("approval");
            j.claim(&op, 10).expect("dispatch");
            assert!(matches!(Journal::open(&file), Err(JournalError::Busy)));
        }
        let mut recovered = Journal::open(&file).expect("reopen");
        assert_eq!(
            recovered.state("alice", "op").expect("state"),
            Some("unknown".into())
        );
        assert!(matches!(
            recovered.claim(&op, 11),
            Err(JournalError::AlreadyDispatched)
        ));
        assert_eq!(recovered.state("bob", "op").expect("no leak"), None);
    }
    #[test]
    fn intent_recovers_but_changed_parameters_do_not_inherit_approval() {
        let dir = tempfile::tempdir().expect("temp");
        let file = dir.path().join("state.sqlite");
        let hash = "a".repeat(64);
        let op = operation(&hash);
        {
            let mut j = Journal::open(&file).expect("open");
            j.register_run("alice", "run", "policy-v1").expect("run");
            j.commit_response(
                "alice",
                "run",
                "inv",
                ResponseAuthority {
                    current: identity(),
                    incoming: identity(),
                    complete: true,
                },
                &hash,
            )
            .expect("response");
            j.prepare(&op).expect("intent");
            j.approve(&op, "alice", 100).expect("approval");
        }
        let mut j = Journal::open(&file).expect("reopen");
        let other = "b".repeat(64);
        assert!(matches!(
            j.claim(&operation(&other), 10),
            Err(JournalError::Conflict)
        ));
        j.claim(&op, 10).expect("dispatch original");
        j.complete("alice", "op", &hash).expect("complete");
        drop(j);
        let mut j = Journal::open(&file).expect("reopen");
        assert_eq!(
            j.state("alice", "op").expect("state"),
            Some("succeeded".into())
        );
        assert!(j.claim(&op, 11).is_err());
    }
}
