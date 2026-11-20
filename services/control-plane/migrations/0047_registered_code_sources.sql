-- The absence of opt-in retains the existing no-third-party-connector boundary.
CREATE TABLE cloud_external_source_opt_ins (
    run_id text PRIMARY KEY REFERENCES cloud_runs(run_id),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_source_opt_in_immutable BEFORE UPDATE OR DELETE ON cloud_external_source_opt_ins
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE repository_source_grants (
    grant_id text PRIMARY KEY,
    run_id text NOT NULL REFERENCES cloud_external_source_opt_ins(run_id),
    provider text NOT NULL CHECK(provider IN ('github','gitlab')),
    source_id text NOT NULL,
    source_digest text NOT NULL CHECK(source_digest ~ '^[a-f0-9]{64}$'),
    repository_ids text[] NOT NULL CHECK(cardinality(repository_ids) BETWEEN 1 AND 100),
    request_key text NOT NULL,
    request_digest text NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),
    expires_at timestamptz NOT NULL,
    UNIQUE(run_id,provider,request_key), UNIQUE(run_id,provider,grant_id)
);
CREATE TABLE repository_source_revocations (
    grant_id text PRIMARY KEY REFERENCES repository_source_grants(grant_id),
    revoked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE repository_source_calls (
    run_id text NOT NULL,provider text NOT NULL,grant_id text NOT NULL,
    request_key text NOT NULL,request_digest text NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),
    until_at timestamptz NOT NULL,finished_at timestamptz,
    PRIMARY KEY(run_id,provider,request_key),UNIQUE(run_id,provider,request_key,grant_id),
    FOREIGN KEY(run_id,provider,grant_id) REFERENCES repository_source_grants(run_id,provider,grant_id)
);
CREATE TABLE repository_file_receipts (
    artifact_id text PRIMARY KEY,run_id text NOT NULL,provider text NOT NULL,grant_id text NOT NULL,request_key text NOT NULL,
    repository_id text NOT NULL,path text NOT NULL,commit_id text NOT NULL,blob_id text NOT NULL,
    size_bytes bigint NOT NULL CHECK(size_bytes BETWEEN 0 AND 20971520),content_sha256 text NOT NULL CHECK(content_sha256 ~ '^[a-f0-9]{64}$'),
    UNIQUE(run_id,provider,request_key),
    FOREIGN KEY(run_id,provider,request_key,grant_id) REFERENCES repository_source_calls(run_id,provider,request_key,grant_id)
);
CREATE TRIGGER repo_grant_immutable BEFORE UPDATE OR DELETE ON repository_source_grants FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER repo_revoke_immutable BEFORE UPDATE OR DELETE ON repository_source_revocations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER repo_file_receipt_immutable BEFORE UPDATE OR DELETE ON repository_file_receipts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE OR REPLACE VIEW connector_run_calls AS
SELECT run_id,until_at,finished_at FROM pg_view_calls UNION ALL
SELECT run_id,until_at,finished_at FROM s3_source_calls UNION ALL
SELECT run_id,until_at,finished_at FROM repository_source_calls;
CREATE TRIGGER repo_source_call_capacity BEFORE INSERT ON repository_source_calls
FOR EACH ROW EXECUTE FUNCTION check_connector_run_limits();
ALTER TABLE connector_credential_faults DROP CONSTRAINT connector_credential_faults_connector_kind_check;
ALTER TABLE connector_credential_faults ADD CHECK(connector_kind IN ('postgres','s3','github','gitlab'));
