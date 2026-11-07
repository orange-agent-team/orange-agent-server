ALTER TABLE worker_sessions ADD COLUMN renewal_version INTEGER NOT NULL DEFAULT 0 CHECK(renewal_version BETWEEN 0 AND 1);
ALTER TABLE worker_execution_attempts ADD COLUMN renewal_sequence BIGINT NOT NULL DEFAULT 0 CHECK(renewal_sequence>=0);
ALTER TABLE worker_execution_attempts ADD COLUMN supervisor_instance_id TEXT;
CREATE TABLE worker_execution_renewals (
 attempt_id TEXT NOT NULL REFERENCES worker_execution_attempts(attempt_id),
 request_id TEXT NOT NULL,
 sequence BIGINT NOT NULL CHECK(sequence>0),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 budget_ms BIGINT NOT NULL CHECK(budget_ms>1000 AND budget_ms<=45000),
 expires_at TIMESTAMPTZ NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(attempt_id,request_id), UNIQUE(attempt_id,sequence)
);
CREATE TRIGGER execution_renewals_immutable BEFORE UPDATE OR DELETE ON worker_execution_renewals
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
