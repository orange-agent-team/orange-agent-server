-- Device observations are immutable historical evidence. No lease, price or execution state is modified here.
CREATE TABLE worker_transcription_profile_observations (
    observation_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(node_id),
    session_id TEXT NOT NULL,
    connection_epoch BIGINT NOT NULL CHECK (connection_epoch > 0),
    request_id TEXT NOT NULL,
    execution_profile_id TEXT NOT NULL REFERENCES reviewed_execution_profiles(execution_profile_id),
    execution_profile_digest TEXT NOT NULL CHECK (execution_profile_digest ~ '^[0-9a-f]{64}$'),
    observation_digest TEXT NOT NULL CHECK (observation_digest ~ '^[0-9a-f]{64}$'),
    model_version TEXT NOT NULL,
    model_sha256 TEXT NOT NULL CHECK (model_sha256 ~ '^[0-9a-f]{64}$'),
    sample_sha256 TEXT NOT NULL CHECK (sample_sha256 ~ '^[0-9a-f]{64}$'),
    transcript_sha256 TEXT NOT NULL CHECK (transcript_sha256 ~ '^[0-9a-f]{64}$'),
    duration_ms BIGINT NOT NULL CHECK(duration_ms > 0),
    normalized_samples BIGINT NOT NULL CHECK(normalized_samples > 0),
    compute_type TEXT NOT NULL CHECK(compute_type IN ('int8','int8_float32')),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (node_id,connection_epoch,request_id),
    UNIQUE (node_id,connection_epoch,execution_profile_id)
);
CREATE TRIGGER worker_transcription_observation_immutable BEFORE UPDATE OR DELETE
ON worker_transcription_profile_observations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
