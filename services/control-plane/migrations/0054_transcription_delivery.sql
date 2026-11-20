ALTER TABLE worker_sessions ADD COLUMN transcription_output_version INTEGER NOT NULL DEFAULT 0
 CHECK(transcription_output_version IN (0,1) AND (transcription_output_version=0 OR (transcription_input_version=1 AND output_version=1)));
CREATE OR REPLACE FUNCTION reset_transcription_capability() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.connection_epoch<>OLD.connection_epoch OR NEW.session_id<>OLD.session_id THEN
    NEW.transcription_input_version=0;
    NEW.transcription_output_version=0;
 END IF;
 RETURN NEW;
END $$;

CREATE TABLE transcription_response_facts (
 job_id TEXT PRIMARY KEY REFERENCES job_response_commits(job_id),
 owner_id TEXT NOT NULL, grant_id TEXT NOT NULL,
 observation_id TEXT NOT NULL REFERENCES worker_transcription_profile_observations(observation_id),
 source_artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
 source_version_id TEXT NOT NULL, source_sha256 TEXT NOT NULL CHECK (source_sha256 ~ '^[a-f0-9]{64}$'),
 source_bytes BIGINT NOT NULL CHECK(source_bytes BETWEEN 1 AND 100000000),
 duration_ms BIGINT NOT NULL CHECK(duration_ms BETWEEN 1 AND 600000),
 projection_digest TEXT NOT NULL CHECK(projection_digest ~ '^[a-f0-9]{64}$'),
 response_digest TEXT NOT NULL CHECK(response_digest ~ '^[a-f0-9]{64}$'),
 FOREIGN KEY(owner_id,grant_id) REFERENCES transcription_input_grants(owner_id,grant_id)
);
CREATE TRIGGER transcription_fact_immutable BEFORE UPDATE OR DELETE ON transcription_response_facts
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE VIEW result_source_bindings AS
SELECT job_id,owner_id,artifact_id,version_id,artifact_sha256,size_bytes FROM job_inputs
UNION ALL SELECT job_id,owner_id,source_artifact_id,source_version_id,source_sha256,source_bytes FROM transcription_response_facts;
