-- Only one original cutoff for the assigned input. A subsequent renewal, restart
-- or new request cannot lengthen a signed capability issued under an older lease.
CREATE TABLE worker_input_download_grants(
    attempt_id TEXT NOT NULL REFERENCES worker_execution_attempts(attempt_id),
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    version_id TEXT NOT NULL REFERENCES artifacts(version_id),
    session_id TEXT NOT NULL,
    node_id TEXT NOT NULL REFERENCES nodes(node_id),
    certificate_sha256 TEXT NOT NULL CHECK(certificate_sha256 ~ '^[0-9a-f]{64}$'),
    command_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    connection_epoch BIGINT NOT NULL CHECK(connection_epoch>0),
    fencing_token BIGINT NOT NULL CHECK(fencing_token>0),
    provider_version TEXT NOT NULL CHECK(provider_version<>'null' AND octet_length(provider_version) BETWEEN 1 AND 1024),
    expires_unix BIGINT NOT NULL,
    PRIMARY KEY(attempt_id,artifact_id)
);
CREATE TRIGGER immutable_worker_input_download BEFORE UPDATE OR DELETE ON worker_input_download_grants
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
