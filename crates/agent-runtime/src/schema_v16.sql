-- A Run may edit only with an explicitly pinned separate edit consent.
CREATE TABLE runtime_edit_capabilities(
 run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),
 consent_id TEXT NOT NULL REFERENCES editing_consents(consent_id),
 consent_version INTEGER NOT NULL CHECK(consent_version>0),environment TEXT NOT NULL
);
CREATE TRIGGER runtime_edit_no_update BEFORE UPDATE ON runtime_edit_capabilities BEGIN SELECT RAISE(ABORT,'immutable Run edit capability');END;
CREATE TRIGGER runtime_edit_no_delete BEFORE DELETE ON runtime_edit_capabilities BEGIN SELECT RAISE(ABORT,'Run edit capability retained');END;
PRAGMA user_version=16;
