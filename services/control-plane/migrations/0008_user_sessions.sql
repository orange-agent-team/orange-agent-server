CREATE TABLE user_auth_flows (
  state_hash TEXT PRIMARY KEY,
  browser_hash TEXT NOT NULL,
  nonce TEXT NOT NULL,
  pkce_verifier TEXT NOT NULL,
  expires_at BIGINT NOT NULL
);
CREATE TABLE user_sessions (
  token_hash TEXT PRIMARY KEY,
  session_id TEXT NOT NULL UNIQUE,
  user_id TEXT NOT NULL REFERENCES users(user_id),
  csrf_token TEXT NOT NULL,
  auth_time BIGINT NOT NULL,
  idle_expires_at BIGINT NOT NULL,
  absolute_expires_at BIGINT NOT NULL,
  revoked BOOLEAN NOT NULL DEFAULT FALSE,
  CHECK(idle_expires_at<=absolute_expires_at)
);
CREATE INDEX user_session_principal ON user_sessions(user_id);
CREATE TABLE user_auth_audit (
  event_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(user_id),
  session_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  occurred_at BIGINT NOT NULL
);
CREATE TRIGGER user_auth_audit_immutable BEFORE UPDATE OR DELETE ON user_auth_audit
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
