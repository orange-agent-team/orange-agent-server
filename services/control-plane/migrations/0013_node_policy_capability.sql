ALTER TABLE worker_sessions DROP CONSTRAINT worker_sessions_node_control_version_check;
ALTER TABLE worker_sessions ADD CONSTRAINT worker_sessions_node_control_version_check CHECK(node_control_version IN(0,1,2));
