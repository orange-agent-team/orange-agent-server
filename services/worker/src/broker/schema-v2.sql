ALTER TABLE broker_meta ADD COLUMN enrolled_node_id TEXT;
ALTER TABLE broker_meta ADD COLUMN node_policy_generation INTEGER NOT NULL DEFAULT 0 CHECK(node_policy_generation>=0);
ALTER TABLE broker_meta ADD COLUMN platform_blocked INTEGER NOT NULL DEFAULT 0 CHECK(platform_blocked IN(0,1));
CREATE TABLE node_policy_commands (
  command_id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL,
  generation INTEGER NOT NULL UNIQUE CHECK(generation>0),
  payload_digest TEXT NOT NULL,
  source_receipt_id TEXT NOT NULL,
  result TEXT NOT NULL
);
CREATE TRIGGER node_policy_no_update BEFORE UPDATE ON node_policy_commands BEGIN SELECT RAISE(ABORT,'immutable node policy result'); END;
CREATE TRIGGER node_policy_no_delete BEFORE DELETE ON node_policy_commands BEGIN SELECT RAISE(ABORT,'immutable node policy result'); END;
PRAGMA user_version=2;
