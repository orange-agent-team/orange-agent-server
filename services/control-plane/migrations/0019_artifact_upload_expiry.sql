-- Active metadata quota is separate from retained immutable ID history.
UPDATE artifact_capacity c SET records=(SELECT count(*) FROM artifacts a WHERE a.root_id=c.root_id AND a.state!='deleted');
CREATE TABLE artifact_expirations(
 artifact_id TEXT PRIMARY KEY REFERENCES artifacts(artifact_id),
 reason TEXT NOT NULL CHECK(reason='upload_expired'),
 occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER artifact_expiration_immutable BEFORE UPDATE OR DELETE ON artifact_expirations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
