-- Physical and consent identities captured at Run registration, never inferred
-- retroactively from a later directory selection.
CREATE TABLE runtime_workspace_roots(run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),owner TEXT NOT NULL,environment TEXT NOT NULL,policy TEXT NOT NULL,resource TEXT NOT NULL,workspace_id TEXT REFERENCES local_workspaces(workspace_id),consent_version INTEGER,CHECK((workspace_id IS NULL AND consent_version IS NULL) OR(workspace_id IS NOT NULL AND consent_version>0)));
CREATE TABLE runtime_memory_lineage(operation_id TEXT PRIMARY KEY REFERENCES runtime_memory_operations(operation_id),body TEXT NOT NULL CHECK(json_valid(body)),digest TEXT NOT NULL,approval_digest TEXT);
CREATE TRIGGER memory_root_no_update BEFORE UPDATE ON runtime_workspace_roots BEGIN SELECT RAISE(ABORT,'immutable Run source root');END;
CREATE TRIGGER memory_root_no_delete BEFORE DELETE ON runtime_workspace_roots BEGIN SELECT RAISE(ABORT,'Run source root retained');END;
CREATE TRIGGER memory_lineage_binding BEFORE UPDATE ON runtime_memory_lineage WHEN NEW.operation_id!=OLD.operation_id OR NEW.body!=OLD.body OR NEW.digest!=OLD.digest OR OLD.approval_digest IS NOT NULL OR NEW.approval_digest IS NULL BEGIN SELECT RAISE(ABORT,'immutable source lineage');END;
CREATE TRIGGER memory_lineage_no_delete BEFORE DELETE ON runtime_memory_lineage BEGIN SELECT RAISE(ABORT,'source lineage retained');END;
PRAGMA user_version=24;
