ALTER TABLE repository_source_grants DROP CONSTRAINT repository_source_grants_provider_check;
ALTER TABLE repository_source_grants ADD CHECK(provider IN ('github','gitlab','notion','confluence'));
ALTER TABLE connector_credential_faults DROP CONSTRAINT connector_credential_faults_connector_kind_check;
ALTER TABLE connector_credential_faults ADD CHECK(connector_kind IN ('postgres','s3','github','gitlab','notion','confluence'));
CREATE OR REPLACE FUNCTION require_external_source_opt_in() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.provider IN ('github','gitlab') THEN
        IF EXISTS(SELECT FROM cloud_external_source_opt_ins WHERE run_id=NEW.run_id) THEN RETURN NEW; END IF;
    ELSIF NEW.provider IN ('notion','confluence') THEN
        IF EXISTS(SELECT FROM cloud_document_source_opt_ins WHERE run_id=NEW.run_id) THEN RETURN NEW; END IF;
    END IF;
    RAISE EXCEPTION 'run missing provider-specific external access consent';
END $$;

CREATE TABLE confluence_quarantine (
    artifact_id text PRIMARY KEY,run_id text NOT NULL,owner_id text NOT NULL REFERENCES owners(owner_id),
    grant_id text NOT NULL,provider text NOT NULL DEFAULT 'confluence' CHECK(provider='confluence'),request_key text NOT NULL,
    root_id text NOT NULL REFERENCES artifact_capacity(root_id),space_key text NOT NULL,
    storage_key text NOT NULL UNIQUE CHECK(storage_key='quarantine-'||artifact_id),
    meta jsonb NOT NULL CHECK(jsonb_typeof(meta)='object'),size_bytes bigint NOT NULL CHECK(size_bytes BETWEEN 0 AND 20971520),
    sha256 text CHECK(sha256 ~ '^[a-f0-9]{64}$'),
    state text NOT NULL CHECK(state IN ('staging','staged','available','purging','purged')),
    stage_until timestamptz NOT NULL,retain_until timestamptz NOT NULL,
    UNIQUE(run_id,request_key),
    FOREIGN KEY(run_id,provider,request_key,grant_id) REFERENCES repository_source_calls(run_id,provider,request_key,grant_id),
    CHECK(state NOT IN ('staged','available') OR sha256 IS NOT NULL)
);
CREATE INDEX confluence_quarantine_pending ON confluence_quarantine(root_id,state,stage_until) WHERE state<>'purged';
CREATE FUNCTION confluence_quarantine_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (to_jsonb(NEW)-'state'-'sha256')<>(to_jsonb(OLD)-'state'-'sha256') OR (OLD.sha256 IS NOT NULL AND NEW.sha256 IS DISTINCT FROM OLD.sha256)
       OR NOT ((OLD.state='staging' AND NEW.state IN ('staged','purging')) OR
               (OLD.state='staged' AND NEW.state IN ('available','purging')) OR
               (OLD.state='available' AND NEW.state='purging') OR (OLD.state='purging' AND NEW.state='purged')) THEN
        RAISE EXCEPTION 'invalid quarantine transition';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER confluence_quarantine_state BEFORE UPDATE ON confluence_quarantine FOR EACH ROW EXECUTE FUNCTION confluence_quarantine_transition();
CREATE TRIGGER confluence_quarantine_no_delete BEFORE DELETE ON confluence_quarantine FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE VIEW confluence_quarantine_authority AS
SELECT q.artifact_id FROM confluence_quarantine q JOIN cloud_runs r ON r.run_id=q.run_id AND r.owner_id=q.owner_id
JOIN owners own ON own.owner_id=q.owner_id AND own.state='active'
JOIN users person ON person.user_id=r.user_id AND person.personal_owner_id=q.owner_id AND person.active
JOIN cloud_document_source_opt_ins o ON o.run_id=q.run_id
JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id AND src.owner_id=r.owner_id
JOIN external_source_grants g ON g.run_id=q.run_id AND g.grant_id=q.grant_id AND g.provider=q.provider
WHERE r.status IN ('running','waiting_approval','paused') AND src.state='available' AND g.expires_at>clock_timestamp() AND q.retain_until>clock_timestamp()
AND NOT EXISTS(SELECT FROM repository_source_revocations v WHERE v.grant_id=g.grant_id)
AND NOT EXISTS(SELECT FROM connector_credential_faults f WHERE f.source_digest=g.source_digest);
