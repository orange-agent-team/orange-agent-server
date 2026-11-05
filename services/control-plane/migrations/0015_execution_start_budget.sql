-- Capability2 enables a receiver anchored to its FIRST ACK send. Persisting the
-- original budget makes duplicate ACK responses immutable, never fresh leases.
ALTER TABLE worker_sessions DROP CONSTRAINT worker_sessions_execution_control_version_check;
ALTER TABLE worker_sessions ADD CONSTRAINT worker_sessions_execution_control_version_check CHECK(execution_control_version BETWEEN 0 AND 2);
ALTER TABLE worker_execution_attempts ADD COLUMN start_budget_ms BIGINT CHECK(start_budget_ms>1000 AND start_budget_ms<=45000);
