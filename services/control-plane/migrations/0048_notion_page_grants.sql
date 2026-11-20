-- A repository-only Run never gains consent to send a query to a document provider.
CREATE TABLE cloud_document_source_opt_ins (
    run_id text PRIMARY KEY REFERENCES cloud_runs(run_id),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_document_source_opt_in_immutable BEFORE UPDATE OR DELETE ON cloud_document_source_opt_ins
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- Retain the physical names used by previous binaries and their immutable evidence.
ALTER TABLE repository_source_grants DROP CONSTRAINT repository_source_grants_run_id_fkey;
ALTER TABLE repository_source_grants ADD FOREIGN KEY(run_id) REFERENCES cloud_runs(run_id);
ALTER TABLE repository_source_grants DROP CONSTRAINT repository_source_grants_provider_check;
ALTER TABLE repository_source_grants ADD CHECK (provider IN ('github','gitlab','notion'));
ALTER TABLE connector_credential_faults DROP CONSTRAINT connector_credential_faults_connector_kind_check;
ALTER TABLE connector_credential_faults ADD CHECK(connector_kind IN ('postgres','s3','github','gitlab','notion'));

CREATE FUNCTION require_external_source_opt_in() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.provider IN ('github','gitlab') THEN
        IF EXISTS(SELECT FROM cloud_external_source_opt_ins WHERE run_id=NEW.run_id) THEN RETURN NEW; END IF;
    ELSIF NEW.provider='notion' THEN
        IF EXISTS(SELECT FROM cloud_document_source_opt_ins WHERE run_id=NEW.run_id) THEN RETURN NEW; END IF;
    END IF;
    RAISE EXCEPTION 'run missing provider-specific external access consent';
END $$;
CREATE TRIGGER external_source_grant_egress BEFORE INSERT ON repository_source_grants
FOR EACH ROW EXECUTE FUNCTION require_external_source_opt_in();

CREATE VIEW external_source_grants AS
    SELECT grant_id,run_id,provider,source_id,source_digest,repository_ids AS root_keys,
           request_key,request_digest,expires_at
    FROM repository_source_grants;
