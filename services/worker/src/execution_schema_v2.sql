CREATE TABLE execution_rejections(
 command_id TEXT PRIMARY KEY REFERENCES execution_commands(command_id),
 result INTEGER NOT NULL CHECK(result IN(2,3)),ack BLOB NOT NULL,receipt BLOB
);
CREATE TRIGGER rejection_immutable BEFORE UPDATE ON execution_rejections WHEN OLD.command_id!=NEW.command_id OR OLD.result!=NEW.result OR OLD.ack!=NEW.ack OR (OLD.receipt IS NOT NULL AND OLD.receipt IS NOT NEW.receipt)
BEGIN SELECT RAISE(ABORT,'immutable rejection decision/receipt'); END;
CREATE TRIGGER rejection_no_delete BEFORE DELETE ON execution_rejections BEGIN SELECT RAISE(ABORT,'rejection history cannot be deleted'); END;
-- v1 did not implement a negative-decision writer. Never invent one from a
-- legacy state label without its original ACK evidence.
UPDATE execution_commands SET state='unknown' WHERE state='rejected';
PRAGMA user_version=2;
