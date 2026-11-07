ALTER TABLE worker_sessions ADD COLUMN output_version INTEGER NOT NULL DEFAULT 0 CHECK(output_version BETWEEN 0 AND 1);
-- Output bytes use the same single writer, quota and orphan lifecycle. The
-- separate binding prevents user upload endpoints from supplying/replacing them.
CREATE TABLE worker_output_uploads (
 job_id TEXT PRIMARY KEY REFERENCES jobs(job_id),
 attempt_id TEXT NOT NULL UNIQUE REFERENCES worker_execution_attempts(attempt_id),
 node_id TEXT NOT NULL REFERENCES nodes(node_id),
 upload_id TEXT NOT NULL UNIQUE,
 artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(artifact_id),
 header_digest TEXT NOT NULL CHECK(header_digest ~ '^[0-9a-f]{64}$'),
 response_digest TEXT NOT NULL CHECK(response_digest ~ '^[0-9a-f]{64}$'),
 original_header BYTEA NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER worker_output_identity BEFORE UPDATE OR DELETE ON worker_output_uploads
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE job_response_commits (
 job_id TEXT PRIMARY KEY REFERENCES jobs(job_id),
 commit_id TEXT NOT NULL UNIQUE,
 artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(artifact_id),
 version_id TEXT NOT NULL REFERENCES artifacts(version_id),
 response_digest TEXT NOT NULL CHECK(response_digest ~ '^[0-9a-f]{64}$'),
 receipt BYTEA NOT NULL,
 committed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER response_commit_immutable BEFORE UPDATE OR DELETE ON job_response_commits
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
