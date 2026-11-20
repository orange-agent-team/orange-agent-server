-- The interpreter digest is part of the independently approved execution-profile digest.
-- Older immutable observations can continue to be read without acquiring a cleanup identity.
CREATE TABLE transcription_process_identities(
    observation_id TEXT PRIMARY KEY REFERENCES worker_transcription_profile_observations(observation_id),
    python_sha256 TEXT NOT NULL CHECK(python_sha256 ~ '^[0-9a-f]{64}$')
);
CREATE TRIGGER immutable_transcription_process_identity BEFORE UPDATE OR DELETE ON transcription_process_identities
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
