-- Public certificate material is prepared by a dedicated development signer and activated at redemption.
CREATE TABLE worker_enrollment_certificates (
    request_id uuid PRIMARY KEY REFERENCES worker_enrollment_grants(request_id) ON DELETE CASCADE,
    node_id text NOT NULL UNIQUE,
    region_id text NOT NULL,
    certificate_sha256 text NOT NULL UNIQUE CHECK(certificate_sha256 ~ '^[0-9a-f]{64}$'),
    issuer_sha256 text NOT NULL CHECK(issuer_sha256 ~ '^[0-9a-f]{64}$'),
    leaf_pem text NOT NULL CHECK(octet_length(leaf_pem) BETWEEN 100 AND 16384),
    not_before timestamptz NOT NULL,
    expires_at timestamptz NOT NULL CHECK(expires_at>not_before AND expires_at<=not_before+interval '24 hours 1 minute'),
    redemption_id uuid UNIQUE,
    redeemed_at timestamptz,
    CHECK((redemption_id IS NULL)=(redeemed_at IS NULL))
);
CREATE FUNCTION guard_worker_enrollment_certificate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND OLD.redemption_id IS NULL AND NEW.redemption_id IS NOT NULL AND NEW.redeemed_at IS NOT NULL
        AND (to_jsonb(OLD)-ARRAY['redemption_id','redeemed_at'])=(to_jsonb(NEW)-ARRAY['redemption_id','redeemed_at'])
        THEN RETURN NEW;
    END IF;
    IF TG_OP='DELETE' AND OLD.expires_at<=clock_timestamp()-interval '28 days' THEN RETURN OLD; END IF;
    RAISE EXCEPTION 'certificate issuance is immutable' USING ERRCODE='23514';
END $$;
CREATE TRIGGER protect_worker_enrollment_certificate BEFORE UPDATE OR DELETE ON worker_enrollment_certificates
    FOR EACH ROW EXECUTE FUNCTION guard_worker_enrollment_certificate();
