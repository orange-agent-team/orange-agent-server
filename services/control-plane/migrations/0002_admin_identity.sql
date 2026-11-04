CREATE TABLE admin_principals (
  principal_id TEXT PRIMARY KEY,
  issuer TEXT NOT NULL,
  subject TEXT NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  policy_version BIGINT NOT NULL DEFAULT 1 CHECK(policy_version > 0),
  UNIQUE(issuer,subject)
);
CREATE TABLE admin_grants (
  principal_id TEXT NOT NULL REFERENCES admin_principals(principal_id),
  role TEXT NOT NULL CHECK(role IN ('platform_admin','support_operator','compute_operator','finance_operator','security_operator','release_operator','auditor')),
  environment TEXT NOT NULL CHECK(environment IN ('dev','staging','production')),
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('platform','region','tenant','resource')),
  scope_id TEXT NOT NULL,
  expires_at BIGINT NOT NULL,
  revoked BOOLEAN NOT NULL DEFAULT FALSE,
  PRIMARY KEY(principal_id,role,environment,scope_kind,scope_id)
);
CREATE TABLE admin_auth_flows (
  state_hash TEXT PRIMARY KEY,
  browser_hash TEXT NOT NULL,
  nonce TEXT NOT NULL,
  pkce_verifier TEXT NOT NULL,
  expires_at BIGINT NOT NULL,
  existing_session_hash TEXT,
  expected_principal TEXT REFERENCES admin_principals(principal_id)
);
CREATE TABLE admin_sessions (
  token_hash TEXT PRIMARY KEY,
  principal_id TEXT NOT NULL REFERENCES admin_principals(principal_id),
  csrf_token TEXT NOT NULL,
  auth_time BIGINT NOT NULL,
  idle_expires_at BIGINT NOT NULL,
  absolute_expires_at BIGINT NOT NULL,
  revoked BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX admin_session_principal ON admin_sessions(principal_id);
CREATE TABLE admin_audit (
  event_id TEXT PRIMARY KEY,
  principal_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  scope_environment TEXT,
  scope_kind TEXT,
  scope_id TEXT,
  entity_id TEXT,
  occurred_at BIGINT NOT NULL
);
CREATE TRIGGER admin_audit_immutable BEFORE UPDATE OR DELETE ON admin_audit
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
-- Read model for operations; creation/execution is implemented in a later slice.
CREATE TABLE admin_actions (
  action_id TEXT PRIMARY KEY,
  environment TEXT NOT NULL,
  scope_kind TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  action_type TEXT NOT NULL,
  document JSONB NOT NULL,
  CHECK(document->>'action_id'=action_id),
  CHECK(document->'request'->>'action_type'=action_type),
  CHECK(document->'request'->'scope'->>'environment'=environment),
  CHECK(document->'request'->'scope'->>'kind'=scope_kind),
  CHECK(document->'request'->'scope'->>'scope_id'=scope_id)
);
CREATE INDEX admin_actions_scope ON admin_actions(environment,scope_kind,scope_id,action_id);
