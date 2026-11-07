ALTER TABLE worker_sessions ADD COLUMN cpu_cleanup_version INTEGER NOT NULL DEFAULT 0 CHECK(cpu_cleanup_version BETWEEN 0 AND 1);
CREATE TABLE reviewed_cpu_profiles(
 execution_profile_id TEXT PRIMARY KEY REFERENCES reviewed_execution_profiles(execution_profile_id),
 executable_sha256 TEXT NOT NULL CHECK(executable_sha256 ~ '^[0-9a-f]{64}$'),
 model_bytes BIGINT NOT NULL CHECK(model_bytes>0),context_tokens INTEGER NOT NULL CHECK(context_tokens BETWEEN 256 AND 32768),
 startup_timeout_ms BIGINT NOT NULL CHECK(startup_timeout_ms BETWEEN 1000 AND 120000)
);
CREATE TRIGGER reviewed_cpu_profile_immutable BEFORE UPDATE OR DELETE ON reviewed_cpu_profiles FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE worker_execution_attempts DROP CONSTRAINT worker_execution_attempts_state_check;
ALTER TABLE worker_execution_attempts ADD CONSTRAINT worker_execution_attempts_state_check CHECK(state IN('offered','started','rejected','unknown','closed'));
CREATE TABLE worker_cpu_cleanup_receipts(
 attempt_id TEXT PRIMARY KEY REFERENCES worker_execution_attempts(attempt_id),
 job_id TEXT NOT NULL UNIQUE REFERENCES jobs(job_id),node_id TEXT NOT NULL REFERENCES nodes(node_id),
 model_instance_id TEXT NOT NULL,request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 receipt_id TEXT NOT NULL UNIQUE,receipt BYTEA NOT NULL,evidence JSONB NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),UNIQUE(node_id,model_instance_id)
);
CREATE TRIGGER cpu_cleanup_receipt_immutable BEFORE UPDATE OR DELETE ON worker_cpu_cleanup_receipts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE job_cleanup_releases(
 job_id TEXT PRIMARY KEY REFERENCES jobs(job_id),cleanup_receipt_id TEXT NOT NULL REFERENCES worker_cpu_cleanup_receipts(receipt_id),
 transaction_id TEXT NOT NULL UNIQUE REFERENCES ledger_transactions(transaction_id),released BIGINT NOT NULL CHECK(released>=0),
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cleanup_release_immutable BEFORE UPDATE OR DELETE ON job_cleanup_releases FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
