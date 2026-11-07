CREATE TABLE execution_renewals (
 command_id TEXT NOT NULL REFERENCES execution_commands(command_id),
 request_id TEXT NOT NULL UNIQUE, sequence INTEGER NOT NULL CHECK(sequence>0),
 request_digest TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN('pending','confirmed','unknown')),
 budget_ms INTEGER, expires_seconds INTEGER,
 PRIMARY KEY(command_id,sequence)
);
CREATE TRIGGER renewal_identity BEFORE UPDATE ON execution_renewals WHEN
 OLD.command_id!=NEW.command_id OR OLD.request_id!=NEW.request_id OR OLD.sequence!=NEW.sequence OR OLD.request_digest!=NEW.request_digest
 OR (OLD.state!='pending' AND (OLD.state!=NEW.state OR OLD.budget_ms IS NOT NEW.budget_ms OR OLD.expires_seconds IS NOT NEW.expires_seconds))
BEGIN SELECT RAISE(ABORT,'immutable renewal history'); END;
CREATE TRIGGER renewal_no_delete BEFORE DELETE ON execution_renewals BEGIN SELECT RAISE(ABORT,'renewal history cannot be deleted'); END;
PRAGMA user_version=4;
