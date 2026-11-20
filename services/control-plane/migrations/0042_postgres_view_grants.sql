-- Consent belongs to one authenticated user's run and exact registered source revision.
CREATE TABLE pg_view_grants (
    grant_id text PRIMARY KEY,
    run_id text NOT NULL REFERENCES cloud_runs(run_id),
    source_id text NOT NULL,
    source_digest text NOT NULL CHECK (source_digest ~ '^[a-f0-9]{64}$'),
    view_ids text[] NOT NULL CHECK (cardinality(view_ids) BETWEEN 1 AND 100),
    request_key text NOT NULL,
    request_digest text NOT NULL CHECK (request_digest ~ '^[a-f0-9]{64}$'),
    expires_at timestamptz NOT NULL,
    UNIQUE (run_id,request_key),
    UNIQUE (grant_id,run_id)
);
CREATE TABLE pg_view_revocations (
    grant_id text PRIMARY KEY REFERENCES pg_view_grants(grant_id),
    revoked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE pg_view_calls (
    run_id text NOT NULL,
    request_key text NOT NULL,
    grant_id text NOT NULL,
    until_at timestamptz NOT NULL,
    finished_at timestamptz,
    PRIMARY KEY (run_id,request_key),
    FOREIGN KEY (grant_id,run_id) REFERENCES pg_view_grants(grant_id,run_id)
);
