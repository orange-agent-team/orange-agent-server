CREATE TABLE local_workspaces(
 workspace_id TEXT PRIMARY KEY,owner TEXT NOT NULL,canonical_path TEXT NOT NULL,
 resource_identity TEXT NOT NULL,environment TEXT NOT NULL UNIQUE,policy TEXT NOT NULL,
 active INTEGER NOT NULL CHECK(active IN(0,1)),version INTEGER NOT NULL CHECK(version>0)
);
CREATE UNIQUE INDEX workspace_active_path ON local_workspaces(owner,canonical_path) WHERE active=1;
CREATE TABLE workspace_request_keys(owner TEXT NOT NULL,request_id TEXT NOT NULL,digest TEXT NOT NULL,workspace_id TEXT NOT NULL REFERENCES local_workspaces(workspace_id),PRIMARY KEY(owner,request_id));
CREATE TABLE run_workspaces(run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),workspace_id TEXT NOT NULL REFERENCES local_workspaces(workspace_id));
CREATE TABLE workspace_audit(seq INTEGER PRIMARY KEY AUTOINCREMENT,workspace_id TEXT NOT NULL REFERENCES local_workspaces(workspace_id),owner TEXT NOT NULL,kind TEXT NOT NULL,version INTEGER NOT NULL);
CREATE TRIGGER workspace_audit_no_update BEFORE UPDATE ON workspace_audit BEGIN SELECT RAISE(ABORT,'immutable workspace audit'); END;
CREATE TRIGGER workspace_audit_no_delete BEFORE DELETE ON workspace_audit BEGIN SELECT RAISE(ABORT,'immutable workspace audit'); END;
PRAGMA user_version=3;
