-- A source refusal invalidates this exact operator registration across replicas and restarts.
CREATE TABLE connector_credential_faults (
    source_digest text PRIMARY KEY CHECK(source_digest ~ '^[a-f0-9]{64}$'),
    owner_id text NOT NULL REFERENCES owners(owner_id),
    connector_kind text NOT NULL CHECK(connector_kind IN ('postgres','s3')),
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX connector_faults_by_owner ON connector_credential_faults(owner_id);
CREATE TRIGGER connector_fault_immutable BEFORE UPDATE OR DELETE ON connector_credential_faults
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
