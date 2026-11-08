-- Explicit immutable per-Run create capability. Legacy Runs gain none.
CREATE TABLE runtime_creation_capabilities(
 run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),
 consent_id TEXT NOT NULL REFERENCES creation_consents(consent_id),
 consent_version INTEGER NOT NULL CHECK(consent_version>0),
 environment TEXT NOT NULL
);
CREATE TRIGGER runtime_creation_no_update BEFORE UPDATE ON runtime_creation_capabilities
BEGIN SELECT RAISE(ABORT,'immutable Run creation capability');END;
CREATE TRIGGER runtime_creation_no_delete BEFORE DELETE ON runtime_creation_capabilities
BEGIN SELECT RAISE(ABORT,'Run creation capability retained');END;
PRAGMA user_version=6;
