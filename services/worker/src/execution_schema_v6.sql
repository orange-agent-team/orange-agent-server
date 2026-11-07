CREATE TABLE cpu_cleanup_facts(
 command_id TEXT PRIMARY KEY REFERENCES cpu_runs(command_id),model_instance_id TEXT NOT NULL UNIQUE,
 evidence BLOB NOT NULL,request BLOB,request_digest TEXT,receipt BLOB,
 CHECK((request_digest IS NULL)=(request IS NULL)),CHECK(receipt IS NULL OR request IS NOT NULL)
);
CREATE TRIGGER cpu_cleanup_fact_identity BEFORE UPDATE ON cpu_cleanup_facts WHEN
 OLD.command_id!=NEW.command_id OR OLD.model_instance_id!=NEW.model_instance_id OR OLD.evidence IS NOT NEW.evidence
 OR (OLD.request IS NOT NULL AND (OLD.request IS NOT NEW.request OR OLD.request_digest IS NOT NEW.request_digest))
 OR (OLD.receipt IS NOT NULL AND (OLD.receipt IS NOT NEW.receipt OR OLD.request_digest IS NOT NEW.request_digest))
BEGIN SELECT RAISE(ABORT,'immutable CPU cleanup facts'); END;
CREATE TRIGGER cpu_cleanup_fact_no_delete BEFORE DELETE ON cpu_cleanup_facts BEGIN SELECT RAISE(ABORT,'CPU cleanup history cannot be deleted'); END;
PRAGMA user_version=6;
