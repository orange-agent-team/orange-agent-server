CREATE TABLE execution_outputs(
 command_id TEXT PRIMARY KEY REFERENCES execution_commands(command_id),
 header BLOB NOT NULL,header_digest TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('prepared','unknown','confirmed')),
 receipt BLOB,
 CHECK((state='confirmed')=(receipt IS NOT NULL))
);
CREATE TRIGGER output_identity BEFORE UPDATE ON execution_outputs WHEN
 OLD.command_id!=NEW.command_id OR OLD.header IS NOT NEW.header OR OLD.header_digest!=NEW.header_digest
 OR (OLD.receipt IS NOT NULL AND OLD.receipt IS NOT NEW.receipt)
BEGIN SELECT RAISE(ABORT,'immutable output identity'); END;
CREATE TRIGGER output_no_delete BEFORE DELETE ON execution_outputs BEGIN SELECT RAISE(ABORT,'output history cannot be deleted'); END;
PRAGMA user_version=5;
