ALTER TABLE worker_sessions ADD COLUMN node_control_version INTEGER NOT NULL DEFAULT 0 CHECK(node_control_version IN(0,1));
ALTER TABLE admin_node_commands DROP CONSTRAINT admin_node_commands_state_check;
ALTER TABLE admin_node_commands ADD CONSTRAINT admin_node_commands_state_check CHECK(state IN('queued','dispatched','received','succeeded','failed','unknown'));
ALTER TABLE admin_node_commands ADD COLUMN payload_digest TEXT CHECK(payload_digest ~ '^[0-9a-f]{64}$');
ALTER TABLE admin_node_commands ADD COLUMN dispatch_session_id TEXT;
ALTER TABLE admin_node_commands ADD COLUMN dispatch_connection_epoch BIGINT;
ALTER TABLE admin_node_commands ADD COLUMN dispatched_at TIMESTAMPTZ;
ALTER TABLE admin_node_commands ADD COLUMN receipt_id TEXT;
ALTER TABLE admin_node_commands ADD COLUMN received_at TIMESTAMPTZ;
CREATE UNIQUE INDEX node_receipt_identity ON admin_node_commands(node_id,receipt_id) WHERE receipt_id IS NOT NULL;
CREATE TABLE worker_node_command_events (
  event_id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL REFERENCES nodes(node_id),
  command_id TEXT NOT NULL REFERENCES admin_node_commands(command_id),
  session_id TEXT NOT NULL,
  connection_epoch BIGINT NOT NULL,
  certificate_sha256 TEXT NOT NULL,
  kind TEXT NOT NULL,
  receipt_id TEXT,
  occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER worker_node_command_events_immutable BEFORE UPDATE OR DELETE ON worker_node_command_events
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION protect_node_command_intent() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.command_id IS DISTINCT FROM NEW.command_id OR OLD.action_id IS DISTINCT FROM NEW.action_id
    OR OLD.node_id IS DISTINCT FROM NEW.node_id OR OLD.command_kind IS DISTINCT FROM NEW.command_kind
    OR OLD.policy_generation IS DISTINCT FROM NEW.policy_generation OR OLD.target_version IS DISTINCT FROM NEW.target_version
    OR OLD.created_at IS DISTINCT FROM NEW.created_at
    OR (OLD.payload_digest IS NOT NULL AND OLD.payload_digest IS DISTINCT FROM NEW.payload_digest) THEN
   RAISE EXCEPTION 'Node command identity and intent are immutable';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER node_command_intent_immutable BEFORE UPDATE ON admin_node_commands
FOR EACH ROW EXECUTE FUNCTION protect_node_command_intent();
