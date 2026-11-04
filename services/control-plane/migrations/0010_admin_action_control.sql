-- Separate managed actions from historical/read-only projections. A projection alone
-- can never become an executable command.
CREATE TABLE admin_action_control (
  action_id TEXT PRIMARY KEY REFERENCES admin_actions(action_id),
  expires_at BIGINT NOT NULL,
  revision BIGINT NOT NULL DEFAULT 1 CHECK(revision > 0)
);
CREATE TABLE admin_action_keys (
  principal_id TEXT NOT NULL REFERENCES admin_principals(principal_id),
  request_key TEXT NOT NULL CHECK(length(request_key) BETWEEN 1 AND 128),
  request_digest TEXT NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),
  action_id TEXT NOT NULL REFERENCES admin_action_control(action_id),
  PRIMARY KEY(principal_id, request_key)
);
CREATE TRIGGER admin_action_keys_immutable BEFORE UPDATE OR DELETE ON admin_action_keys
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE admin_action_events (
  event_id TEXT PRIMARY KEY,
  action_id TEXT NOT NULL REFERENCES admin_action_control(action_id),
  principal_id TEXT NOT NULL REFERENCES admin_principals(principal_id),
  kind TEXT NOT NULL,
  reason TEXT NOT NULL,
  document JSONB NOT NULL,
  occurred_at BIGINT NOT NULL
);
CREATE TRIGGER admin_action_events_immutable BEFORE UPDATE OR DELETE ON admin_action_events
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE admin_node_commands (
  command_id TEXT PRIMARY KEY,
  action_id TEXT NOT NULL UNIQUE REFERENCES admin_action_control(action_id),
  node_id TEXT NOT NULL REFERENCES nodes(node_id),
  command_kind TEXT NOT NULL CHECK(command_kind IN ('node.drain','node.quarantine')),
  target_version BIGINT NOT NULL CHECK(target_version > 0),
  state TEXT NOT NULL CHECK(state IN ('queued','dispatched','succeeded','failed','unknown')),
  created_at BIGINT NOT NULL
);
CREATE FUNCTION protect_admin_action_request() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.document->'request' IS DISTINCT FROM NEW.document->'request'
     OR OLD.document->'requested_by' IS DISTINCT FROM NEW.document->'requested_by'
     OR OLD.document->'payload_digest' IS DISTINCT FROM NEW.document->'payload_digest'
     OR OLD.document->'policy_version' IS DISTINCT FROM NEW.document->'policy_version'
     OR OLD.document->'approval_policy' IS DISTINCT FROM NEW.document->'approval_policy'
     OR OLD.document->'required_approvals' IS DISTINCT FROM NEW.document->'required_approvals'
     OR OLD.document->'created_at' IS DISTINCT FROM NEW.document->'created_at'
     OR OLD.document->'expires_at' IS DISTINCT FROM NEW.document->'expires_at' THEN
    RAISE EXCEPTION 'Admin action request and policy are immutable';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER admin_action_request_immutable BEFORE UPDATE ON admin_actions
FOR EACH ROW EXECUTE FUNCTION protect_admin_action_request();
