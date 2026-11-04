-- Control intent generation must not change on a liveness handshake/reconnect.
ALTER TABLE nodes ADD COLUMN policy_generation BIGINT NOT NULL DEFAULT 0 CHECK(policy_generation>=0);
ALTER TABLE admin_node_commands ADD COLUMN policy_generation BIGINT;
-- Prior commands were only queued, never sent by the previous implementation.
-- Give each stored intent an immutable per-node order without changing its ID.
WITH ordered AS (
  SELECT command_id,row_number() OVER(PARTITION BY node_id ORDER BY target_version,created_at,command_id) AS generation
  FROM admin_node_commands
)
UPDATE admin_node_commands c SET policy_generation=o.generation FROM ordered o WHERE c.command_id=o.command_id;
UPDATE nodes n SET policy_generation=c.generation FROM (
  SELECT node_id,max(policy_generation) AS generation FROM admin_node_commands GROUP BY node_id
) c WHERE n.node_id=c.node_id;
ALTER TABLE admin_node_commands ALTER COLUMN policy_generation SET NOT NULL;
ALTER TABLE admin_node_commands ADD CONSTRAINT node_command_generation_positive CHECK(policy_generation>0);
ALTER TABLE admin_node_commands ADD CONSTRAINT node_command_generation_unique UNIQUE(node_id,policy_generation);
