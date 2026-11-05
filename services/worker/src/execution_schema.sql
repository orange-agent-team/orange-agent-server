CREATE TABLE execution_receiver(singleton INTEGER PRIMARY KEY CHECK(singleton=1),node_id TEXT NOT NULL,session_id TEXT NOT NULL,connection_epoch INTEGER NOT NULL);
CREATE TABLE execution_commands(
 command_id TEXT PRIMARY KEY,attempt_id TEXT NOT NULL UNIQUE,journal_id TEXT NOT NULL UNIQUE,
 assignment BLOB NOT NULL,grant_json TEXT,
 state TEXT NOT NULL CHECK(state IN('received','reserved','ack_sent','start_claimed','running','unknown','rejected'))
);
CREATE TRIGGER execution_identity_immutable BEFORE UPDATE ON execution_commands WHEN OLD.command_id!=NEW.command_id OR OLD.attempt_id!=NEW.attempt_id OR OLD.journal_id!=NEW.journal_id OR OLD.assignment!=NEW.assignment OR (OLD.grant_json IS NOT NULL AND OLD.grant_json IS NOT NEW.grant_json)
BEGIN SELECT RAISE(ABORT,'immutable execution identity'); END;
CREATE TRIGGER execution_no_delete BEFORE DELETE ON execution_commands BEGIN SELECT RAISE(ABORT,'execution history cannot be deleted'); END;
PRAGMA user_version=1;
