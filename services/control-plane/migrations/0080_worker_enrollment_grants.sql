CREATE TABLE worker_enrollment_grants (
    request_id uuid PRIMARY KEY,
    owner_id text NOT NULL REFERENCES owners(owner_id),
    requested_by text NOT NULL REFERENCES users(user_id),
    spki_sha256 text NOT NULL CHECK(spki_sha256 ~ '^[0-9a-f]{64}$'),
    csr_sha256 text NOT NULL CHECK(csr_sha256 ~ '^[0-9a-f]{64}$'),
    csr_der bytea NOT NULL CHECK(octet_length(csr_der) BETWEEN 64 AND 3072),
    exchange_token_sha256 text NOT NULL CHECK(exchange_token_sha256 ~ '^[0-9a-f]{64}$'),
    state text NOT NULL DEFAULT 'authorized' CHECK(state IN ('authorized','revoked')),
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL CHECK(expires_at=created_at+interval '10 minutes'),
    revoked_at timestamptz,
    CHECK((state='revoked')=(revoked_at IS NOT NULL)),
    UNIQUE(exchange_token_sha256)
);
CREATE INDEX worker_enrollment_owner_window ON worker_enrollment_grants(owner_id,created_at);
CREATE INDEX worker_enrollment_retention ON worker_enrollment_grants(expires_at,request_id);
CREATE FUNCTION guard_worker_enrollment_grant() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' AND OLD.expires_at<=clock_timestamp()-interval '30 days' THEN RETURN OLD; END IF;
    IF TG_OP='UPDATE' AND OLD.state='authorized' AND NEW.state='revoked' AND NEW.revoked_at>=OLD.created_at
        AND (to_jsonb(OLD)-ARRAY['state','revoked_at'])=(to_jsonb(NEW)-ARRAY['state','revoked_at']) THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'immutable worker enrollment grant' USING ERRCODE='23514';
END $$;
CREATE TRIGGER protect_worker_enrollment_grant BEFORE UPDATE OR DELETE ON worker_enrollment_grants
    FOR EACH ROW EXECUTE FUNCTION guard_worker_enrollment_grant();
