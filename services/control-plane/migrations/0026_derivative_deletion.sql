CREATE TABLE artifact_derivative_deletions(
 artifact_id TEXT PRIMARY KEY REFERENCES artifacts(artifact_id),
 source_artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER derivative_deletion_immutable BEFORE UPDATE OR DELETE ON artifact_derivative_deletions FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
