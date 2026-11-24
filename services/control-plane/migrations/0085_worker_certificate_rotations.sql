-- A rotation is a different authorization from an initial enrollment. A stable node changes keys
-- only after an owner-approved, old-key-signed request and an independently signed replacement CSR.
CREATE TABLE worker_certificate_rotation_requests (
    request_id uuid PRIMARY KEY REFERENCES worker_enrollment_grants ON DELETE CASCADE,
    node_id text NOT NULL REFERENCES nodes,
    previous_request_id uuid NOT NULL CHECK(previous_request_id<>request_id),
    previous_certificate_sha256 text NOT NULL REFERENCES worker_credentials(certificate_sha256),
    intent_sha256 text NOT NULL CHECK(intent_sha256 ~ '^[0-9a-f]{64}$'),
    previous_key_signature bytea NOT NULL CHECK(octet_length(previous_key_signature)=64)
);
CREATE TRIGGER immutable_worker_rotation_request BEFORE UPDATE ON worker_certificate_rotation_requests
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

ALTER TABLE worker_enrollment_certificates DROP CONSTRAINT worker_enrollment_certificates_node_id_key;
ALTER TABLE worker_enrollment_certificates ADD COLUMN rotation_parent text REFERENCES worker_credentials(certificate_sha256);
CREATE UNIQUE INDEX worker_initial_certificate_node ON worker_enrollment_certificates(node_id) WHERE rotation_parent IS NULL;

-- Activation receipts survive expiry and cleanup of short-lived handoffs. One predecessor can be consumed only once.
CREATE TABLE worker_certificate_rotation_receipts (
    previous_certificate_sha256 text PRIMARY KEY REFERENCES worker_credentials(certificate_sha256),
    certificate_sha256 text NOT NULL UNIQUE REFERENCES worker_credentials(certificate_sha256),
    node_id text NOT NULL REFERENCES nodes,
    request_id uuid NOT NULL UNIQUE,
    previous_request_id uuid NOT NULL,
    owner_id text NOT NULL REFERENCES owners,
    approved_by text NOT NULL REFERENCES users,
    activated_at timestamptz NOT NULL,
    overlap_until timestamptz NOT NULL CHECK(overlap_until>activated_at AND overlap_until<=activated_at+interval '2 minutes'),
    CHECK(certificate_sha256<>previous_certificate_sha256)
);
CREATE TRIGGER immutable_worker_rotation_receipt BEFORE UPDATE OR DELETE ON worker_certificate_rotation_receipts
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE VIEW renewable_worker_certificates AS
SELECT c.certificate_sha256,c.node_id,c.expires_at,n.owner_id,n.region_id,e.request_id,e.issuer_sha256,e.leaf_pem,g.spki_sha256
FROM worker_credentials c JOIN nodes n USING(node_id)
JOIN worker_enrollment_certificates e ON e.certificate_sha256=c.certificate_sha256 AND e.node_id=c.node_id
JOIN worker_enrollment_grants g ON g.request_id=e.request_id AND g.owner_id=n.owner_id AND g.state='authorized'
WHERE c.active AND c.not_before<=clock_timestamp() AND c.expires_at>clock_timestamp()
  AND e.redeemed_at IS NOT NULL AND n.state NOT IN ('revoked','quarantined')
  AND worker_issued_leaf_allowed(e.issuer_sha256)
  AND NOT EXISTS(SELECT FROM worker_certificate_rotation_receipts used WHERE used.previous_certificate_sha256=c.certificate_sha256);

CREATE FUNCTION bind_worker_rotation_certificate() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE req worker_certificate_rotation_requests;
BEGIN
    SELECT * INTO req FROM worker_certificate_rotation_requests WHERE request_id=NEW.request_id;
    IF NOT FOUND THEN
        IF NEW.rotation_parent IS NOT NULL THEN RAISE EXCEPTION 'rotation authorization missing'; END IF;
    ELSIF NEW.rotation_parent IS DISTINCT FROM req.previous_certificate_sha256 OR NEW.node_id<>req.node_id OR
          NOT EXISTS(SELECT FROM renewable_worker_certificates src JOIN worker_enrollment_grants g ON g.owner_id=src.owner_id
                WHERE src.certificate_sha256=req.previous_certificate_sha256 AND src.request_id=req.previous_request_id
                AND src.node_id=NEW.node_id AND src.region_id=NEW.region_id AND g.request_id=NEW.request_id
                AND src.spki_sha256<>g.spki_sha256) THEN
        RAISE EXCEPTION 'rotation source changed';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER match_worker_rotation_on_issue BEFORE INSERT ON worker_enrollment_certificates
FOR EACH ROW EXECUTE FUNCTION bind_worker_rotation_certificate();

CREATE FUNCTION activate_worker_rotation() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE expiry timestamptz; cut timestamptz; moment timestamptz;
BEGIN
    -- Same node fence as mTLS session establishment. A concurrent redemption or Hello must recheck the predecessor.
    PERFORM node_id FROM nodes WHERE node_id=NEW.node_id FOR UPDATE;
    SELECT src.expires_at INTO expiry FROM renewable_worker_certificates src
        JOIN worker_enrollment_grants g ON g.request_id=NEW.request_id AND g.owner_id=src.owner_id AND g.state='authorized'
        JOIN worker_certificate_rotation_requests r ON r.request_id=g.request_id AND r.previous_request_id=src.request_id
        WHERE src.certificate_sha256=NEW.rotation_parent AND src.node_id=NEW.node_id AND src.region_id=NEW.region_id;
    moment:=clock_timestamp();
    IF expiry IS NULL OR expiry<=moment THEN RAISE EXCEPTION 'rotation predecessor unavailable'; END IF;
    cut:=least(expiry,moment+interval '2 minutes');
    INSERT INTO worker_certificate_rotation_receipts
        SELECT NEW.rotation_parent,NEW.certificate_sha256,NEW.node_id,NEW.request_id,r.previous_request_id,
            g.owner_id,g.requested_by,moment,cut FROM worker_certificate_rotation_requests r
            JOIN worker_enrollment_grants g USING(request_id) WHERE r.request_id=NEW.request_id;
    -- Existing gateways already check active and expires_at for every effect. Do not depend on a cleanup timer.
    UPDATE worker_credentials SET expires_at=cut WHERE certificate_sha256=NEW.rotation_parent;
    UPDATE worker_credentials c SET active=false FROM worker_certificate_rotation_receipts r
        WHERE r.node_id=NEW.node_id AND c.certificate_sha256=r.previous_certificate_sha256
        AND c.certificate_sha256<>NEW.rotation_parent AND c.active;
    RETURN NEW;
END $$;
CREATE TRIGGER register_worker_rotation AFTER UPDATE OF redeemed_at ON worker_enrollment_certificates
FOR EACH ROW WHEN (OLD.redeemed_at IS NULL AND NEW.redeemed_at IS NOT NULL AND NEW.rotation_parent IS NOT NULL)
EXECUTE FUNCTION activate_worker_rotation();

CREATE FUNCTION retire_predecessor_on_worker_hello() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE worker_credentials c SET active=false FROM worker_certificate_rotation_receipts r
      WHERE r.node_id=NEW.node_id AND r.certificate_sha256=NEW.certificate_sha256 AND c.certificate_sha256=r.previous_certificate_sha256 AND c.active;
    RETURN NEW;
END $$;
CREATE TRIGGER retire_predecessor_on_worker_hello AFTER INSERT OR UPDATE OF certificate_sha256 ON worker_sessions
FOR EACH ROW EXECUTE FUNCTION retire_predecessor_on_worker_hello();

CREATE INDEX rotation_requests_by_parent ON worker_certificate_rotation_requests(previous_request_id);
CREATE FUNCTION revoke_descendant_worker_rotations() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    WITH RECURSIVE children(id) AS (
        SELECT request_id FROM worker_certificate_rotation_requests WHERE previous_request_id=NEW.request_id
        UNION
        SELECT r.request_id FROM worker_certificate_rotation_requests r JOIN children c ON r.previous_request_id=c.id
    )
    UPDATE worker_enrollment_grants g SET state='revoked',revoked_at=clock_timestamp()
       FROM children WHERE g.request_id=children.id AND g.state='authorized';
    RETURN NEW;
END $$;
CREATE TRIGGER revoke_descendant_worker_rotations AFTER UPDATE OF state ON worker_enrollment_grants
FOR EACH ROW WHEN (OLD.state='authorized' AND NEW.state='revoked') EXECUTE FUNCTION revoke_descendant_worker_rotations();
