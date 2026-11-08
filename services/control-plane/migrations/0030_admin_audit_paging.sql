-- Scoped audit reads use a fixed ingestion boundary. Existing records retain
-- their original columns; assigned legacy sequence is not historical chronology.
-- PostgreSQL17 ADD COLUMN/identity semantics: https://www.postgresql.org/docs/17/sql-altertable.html
ALTER TABLE admin_audit ADD COLUMN request_id TEXT CHECK(request_id IS NULL OR (length(request_id)<=128 AND request_id ~ '^[A-Za-z0-9][A-Za-z0-9_.:-]*$'));
ALTER TABLE admin_audit ADD COLUMN audit_sequence BIGINT GENERATED ALWAYS AS IDENTITY;
CREATE UNIQUE INDEX admin_audit_sequence_unique ON admin_audit(audit_sequence);
CREATE INDEX admin_audit_environment_sequence ON admin_audit(scope_environment,audit_sequence DESC);
CREATE INDEX admin_audit_scope_sequence ON admin_audit(scope_environment,scope_kind,scope_id,audit_sequence DESC);
-- Opaque random cursors keep the global sequence and scope fingerprint off the
-- client. Every use still needs the same live session and current authority.
CREATE TABLE admin_audit_cursors (
  cursor_hash TEXT PRIMARY KEY CHECK(cursor_hash ~ '^[0-9a-f]{64}$'),
  session_hash TEXT NOT NULL CHECK(session_hash ~ '^[0-9a-f]{64}$'),
  principal_id TEXT NOT NULL,
  authority TEXT NOT NULL CHECK(authority ~ '^[0-9a-f]{64}$'),
  snapshot_sequence BIGINT NOT NULL CHECK(snapshot_sequence>0),
  before_sequence BIGINT NOT NULL CHECK(before_sequence>0 AND before_sequence<=snapshot_sequence),
  expires_at BIGINT NOT NULL CHECK(expires_at>0)
);
CREATE INDEX admin_audit_cursor_expiry ON admin_audit_cursors(expires_at);
CREATE INDEX admin_audit_cursor_quota ON admin_audit_cursors(principal_id,expires_at);
CREATE TRIGGER admin_audit_cursor_no_update BEFORE UPDATE ON admin_audit_cursors
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
