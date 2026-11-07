CREATE TABLE cpu_runs(
 command_id TEXT PRIMARY KEY REFERENCES execution_commands(command_id),
 instance_id TEXT NOT NULL UNIQUE,request_digest TEXT NOT NULL,profile_digest TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN('starting','generating','computed_unsubmitted','cancelled','failed','authority_lost','cleanup_unknown')),
 response_digest TEXT,cleanup_digest TEXT,error_code TEXT,cleanup_confirmed INTEGER NOT NULL DEFAULT 0 CHECK(cleanup_confirmed IN(0,1))
);
CREATE TRIGGER cpu_run_identity BEFORE UPDATE ON cpu_runs WHEN OLD.command_id!=NEW.command_id OR OLD.instance_id!=NEW.instance_id OR OLD.request_digest!=NEW.request_digest OR OLD.profile_digest!=NEW.profile_digest OR (OLD.response_digest IS NOT NULL AND OLD.response_digest IS NOT NEW.response_digest) OR (OLD.cleanup_digest IS NOT NULL AND OLD.cleanup_digest IS NOT NEW.cleanup_digest)
BEGIN SELECT RAISE(ABORT,'immutable CPU execution facts'); END;
CREATE TRIGGER cpu_run_no_delete BEFORE DELETE ON cpu_runs BEGIN SELECT RAISE(ABORT,'CPU execution history cannot be deleted'); END;
PRAGMA user_version=3;
CREATE TABLE cpu_runtime_binding(singleton INTEGER PRIMARY KEY CHECK(singleton=1),root_path TEXT NOT NULL,device TEXT NOT NULL,inode TEXT NOT NULL);
CREATE TRIGGER cpu_root_immutable BEFORE UPDATE ON cpu_runtime_binding BEGIN SELECT RAISE(ABORT,'CPU runtime root requires explicit recovery migration'); END;
CREATE TRIGGER cpu_root_no_delete BEFORE DELETE ON cpu_runtime_binding BEGIN SELECT RAISE(ABORT,'CPU runtime root requires explicit recovery migration'); END;
