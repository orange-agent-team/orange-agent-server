-- A patch approval refers to the actual reviewed source identity/diff, not only
-- to caller-supplied text. Preview history is immutable and remains auditable.
CREATE TABLE editing_previews(
 operation_id TEXT PRIMARY KEY REFERENCES operations(operation_id),
 body TEXT NOT NULL CHECK(json_valid(body)),digest TEXT NOT NULL CHECK(length(digest)=64)
);
CREATE TABLE editing_approvals(
 operation_id TEXT PRIMARY KEY REFERENCES editing_previews(operation_id),
 preview_digest TEXT NOT NULL CHECK(length(preview_digest)=64),
 consent_id TEXT NOT NULL REFERENCES editing_consents(consent_id),
 consent_version INTEGER NOT NULL CHECK(consent_version>0),read_environment TEXT NOT NULL
);
CREATE TRIGGER editing_preview_no_update BEFORE UPDATE ON editing_previews BEGIN SELECT RAISE(ABORT,'immutable patch preview');END;
CREATE TRIGGER editing_preview_no_delete BEFORE DELETE ON editing_previews BEGIN SELECT RAISE(ABORT,'patch preview history retained');END;
CREATE TRIGGER editing_approval_no_update BEFORE UPDATE ON editing_approvals BEGIN SELECT RAISE(ABORT,'immutable patch approval binding');END;
CREATE TRIGGER editing_approval_no_delete BEFORE DELETE ON editing_approvals BEGIN SELECT RAISE(ABORT,'patch approval history retained');END;
PRAGMA user_version=9;
