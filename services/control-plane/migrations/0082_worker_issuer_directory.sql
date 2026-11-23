-- DER fingerprints remain the historical certificate identity. Record SPKI only from a verified issuer.
CREATE TABLE worker_issuer_public_keys (
    issuer_sha256 text PRIMARY KEY CHECK(issuer_sha256 ~ '^[0-9a-f]{64}$'),
    spki_sha256 text NOT NULL CHECK(spki_sha256 ~ '^[0-9a-f]{64}$'),
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX worker_issuer_signing_key ON worker_issuer_public_keys(spki_sha256,issuer_sha256);
CREATE INDEX worker_certificate_issuer ON worker_enrollment_certificates(issuer_sha256,request_id);
CREATE INDEX worker_region_certificate_issuer ON worker_enrollment_certificates(region_id,issuer_sha256,request_id);
CREATE FUNCTION preserve_worker_issuer_observation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'issuer public key observation is immutable' USING ERRCODE='23514';
END $$;
CREATE TRIGGER preserve_worker_issuer_observation BEFORE UPDATE OR DELETE ON worker_issuer_public_keys
    FOR EACH ROW EXECUTE FUNCTION preserve_worker_issuer_observation();
CREATE TRIGGER preserve_worker_issuer_directory BEFORE TRUNCATE ON worker_issuer_public_keys
    FOR EACH STATEMENT EXECUTE FUNCTION preserve_worker_issuer_observation();
