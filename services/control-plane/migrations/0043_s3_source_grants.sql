CREATE TABLE s3_source_grants (
    grant_id text PRIMARY KEY,
    run_id text NOT NULL REFERENCES cloud_runs(run_id),
    source_id text NOT NULL,
    source_digest text NOT NULL CHECK(source_digest ~ '^[0-9a-f]{64}$'),
    request_key text NOT NULL,
    request_digest text NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
    expires_at timestamptz NOT NULL,
    UNIQUE(run_id,request_key),
    UNIQUE(grant_id,run_id)
);
CREATE TABLE s3_source_revocations (
    grant_id text PRIMARY KEY REFERENCES s3_source_grants(grant_id),
    revoked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE s3_source_calls (
    run_id text NOT NULL,
    request_key text NOT NULL,
    grant_id text NOT NULL,
    request_digest text NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
    until_at timestamptz NOT NULL,
    finished_at timestamptz,
    PRIMARY KEY(run_id,request_key),
    UNIQUE(run_id,request_key,grant_id),
    FOREIGN KEY(grant_id,run_id) REFERENCES s3_source_grants(grant_id,run_id)
);
CREATE TABLE s3_artifact_receipts (
    artifact_id text PRIMARY KEY,
    run_id text NOT NULL,
    request_key text NOT NULL,
    grant_id text NOT NULL,
    head jsonb NOT NULL CHECK(jsonb_typeof(head)='object'),
    content_sha256 text NOT NULL CHECK(content_sha256 ~ '^[0-9a-f]{64}$'),
    UNIQUE(run_id,request_key),
    FOREIGN KEY(run_id,request_key,grant_id) REFERENCES s3_source_calls(run_id,request_key,grant_id)
);
CREATE TRIGGER s3_grant_immutable BEFORE UPDATE OR DELETE ON s3_source_grants
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER s3_revocation_immutable BEFORE UPDATE OR DELETE ON s3_source_revocations
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER s3_receipt_immutable BEFORE UPDATE OR DELETE ON s3_artifact_receipts
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE VIEW connector_run_calls AS
SELECT run_id,until_at,finished_at FROM pg_view_calls UNION ALL
SELECT run_id,until_at,finished_at FROM s3_source_calls;

-- Protect aggregate capacity even during overlap with binaries that count only CN-05.
CREATE TABLE connector_run_fences (
    run_id text PRIMARY KEY REFERENCES cloud_runs(run_id),
    revision bigint NOT NULL
);
CREATE FUNCTION check_connector_run_limits() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE used integer; busy integer;
BEGIN
    PERFORM run_id FROM cloud_runs WHERE run_id=NEW.run_id FOR UPDATE;
    INSERT INTO connector_run_fences(run_id,revision) VALUES(NEW.run_id,1)
      ON CONFLICT(run_id) DO UPDATE SET revision=connector_run_fences.revision+1;
    SELECT count(*), count(*) FILTER(WHERE finished_at IS NULL AND until_at>clock_timestamp())
      INTO used,busy FROM connector_run_calls WHERE run_id=NEW.run_id;
    IF used>=100 OR (busy>=4 AND NEW.finished_at IS NULL AND NEW.until_at>clock_timestamp()) THEN
        RAISE EXCEPTION 'connector_run_limit' USING ERRCODE='P0001';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER pg_view_call_capacity BEFORE INSERT ON pg_view_calls
FOR EACH ROW EXECUTE FUNCTION check_connector_run_limits();
CREATE TRIGGER s3_source_call_capacity BEFORE INSERT ON s3_source_calls
FOR EACH ROW EXECUTE FUNCTION check_connector_run_limits();
