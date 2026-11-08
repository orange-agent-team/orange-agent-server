-- Separate existing-file edit consent. Readonly and create-only records do not
-- gain editing rights; reopening metadata never restores OS access.
CREATE TABLE editing_consents(
 consent_id TEXT PRIMARY KEY, owner TEXT NOT NULL, canonical_path TEXT NOT NULL,
 resource_identity TEXT NOT NULL, environment TEXT NOT NULL UNIQUE, policy TEXT NOT NULL,
 active INTEGER NOT NULL CHECK(active IN(0,1)),version INTEGER NOT NULL CHECK(version>0)
);
CREATE UNIQUE INDEX active_editing_path ON editing_consents(owner,canonical_path) WHERE active=1;
CREATE TABLE editing_request_keys(
 owner TEXT NOT NULL,request_id TEXT NOT NULL,digest TEXT NOT NULL,
 consent_id TEXT NOT NULL REFERENCES editing_consents(consent_id),PRIMARY KEY(owner,request_id)
);
CREATE TABLE editing_consent_audit(
 seq INTEGER PRIMARY KEY AUTOINCREMENT,consent_id TEXT NOT NULL REFERENCES editing_consents(consent_id),
 owner TEXT NOT NULL,kind TEXT NOT NULL,version INTEGER NOT NULL
);
CREATE TRIGGER editing_consent_identity BEFORE UPDATE ON editing_consents
WHEN NEW.consent_id IS NOT OLD.consent_id OR NEW.owner IS NOT OLD.owner
 OR NEW.canonical_path IS NOT OLD.canonical_path OR NEW.resource_identity IS NOT OLD.resource_identity
 OR NEW.environment IS NOT OLD.environment OR NEW.policy IS NOT OLD.policy
 OR NOT(OLD.active=1 AND NEW.active=0 AND NEW.version=OLD.version+1)
BEGIN SELECT RAISE(ABORT,'immutable edit consent or invalid revocation');END;
CREATE TRIGGER editing_consent_no_delete BEFORE DELETE ON editing_consents
BEGIN SELECT RAISE(ABORT,'edit consent history retained');END;
CREATE TRIGGER editing_key_no_update BEFORE UPDATE ON editing_request_keys BEGIN SELECT RAISE(ABORT,'immutable consent request');END;
CREATE TRIGGER editing_key_no_delete BEFORE DELETE ON editing_request_keys BEGIN SELECT RAISE(ABORT,'immutable consent request');END;
CREATE TRIGGER editing_audit_no_update BEFORE UPDATE ON editing_consent_audit BEGIN SELECT RAISE(ABORT,'immutable consent audit');END;
CREATE TRIGGER editing_audit_no_delete BEFORE DELETE ON editing_consent_audit BEGIN SELECT RAISE(ABORT,'immutable consent audit');END;
PRAGMA user_version=8;
