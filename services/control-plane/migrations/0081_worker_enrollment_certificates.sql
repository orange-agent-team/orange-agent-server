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
    prepared_at timestamptz NOT NULL DEFAULT clock_timestamp() CHECK(prepared_at>=not_before AND prepared_at<expires_at),
    redemption_id uuid UNIQUE,
    redeemed_at timestamptz,
    CHECK((redemption_id IS NULL)=(redeemed_at IS NULL)),
    CHECK(redeemed_at IS NULL OR (redeemed_at>=prepared_at AND redeemed_at<expires_at))
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

-- Once a manager loses authority an issued or pending grant cannot revive after a later regrant.
CREATE FUNCTION deactivate_enrolled_leaf() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE worker_credentials c SET active=false
        FROM worker_enrollment_certificates e
        WHERE e.request_id=NEW.request_id AND e.redeemed_at IS NOT NULL
        AND c.certificate_sha256=e.certificate_sha256 AND c.node_id=e.node_id AND c.active;
    RETURN NEW;
END $$;
CREATE TRIGGER revoke_enrolled_leaf AFTER UPDATE OF state ON worker_enrollment_grants
    FOR EACH ROW WHEN (OLD.state='authorized' AND NEW.state='revoked') EXECUTE FUNCTION deactivate_enrolled_leaf();

CREATE FUNCTION withdraw_manager_enrollments() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND NEW.active AND NEW.role IN ('owner','admin')
        AND NEW.organization_id=OLD.organization_id AND NEW.user_id=OLD.user_id THEN RETURN NEW; END IF;
    UPDATE worker_enrollment_grants SET state='revoked',revoked_at=clock_timestamp()
        WHERE owner_id=OLD.organization_id AND requested_by=OLD.user_id AND state='authorized';
    RETURN NULL;
END $$;
CREATE TRIGGER withdraw_lost_worker_manager AFTER UPDATE OR DELETE ON memberships FOR EACH ROW
    WHEN (OLD.active AND OLD.role IN ('owner','admin')) EXECUTE FUNCTION withdraw_manager_enrollments();

CREATE FUNCTION withdraw_disabled_user_enrollments() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE worker_enrollment_grants SET state='revoked',revoked_at=clock_timestamp()
        WHERE requested_by=NEW.user_id AND state='authorized';
    RETURN NEW;
END $$;
CREATE TRIGGER withdraw_disabled_worker_user AFTER UPDATE OF active ON users FOR EACH ROW
    WHEN (OLD.active AND NOT NEW.active) EXECUTE FUNCTION withdraw_disabled_user_enrollments();

CREATE FUNCTION withdraw_disabled_owner_enrollments() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE worker_enrollment_grants SET state='revoked',revoked_at=clock_timestamp()
        WHERE state='authorized' AND (owner_id=NEW.owner_id OR requested_by IN
            (SELECT user_id FROM users WHERE personal_owner_id=NEW.owner_id));
    RETURN NEW;
END $$;
CREATE TRIGGER withdraw_disabled_worker_owner AFTER UPDATE OF state ON owners FOR EACH ROW
    WHEN (OLD.state='active' AND NEW.state!='active') EXECUTE FUNCTION withdraw_disabled_owner_enrollments();

UPDATE worker_enrollment_grants g SET state='revoked',revoked_at=clock_timestamp()
WHERE g.state='authorized' AND NOT EXISTS (
    SELECT FROM users u JOIN owners personal ON personal.owner_id=u.personal_owner_id AND personal.state='active'
    JOIN owners owner_scope ON owner_scope.owner_id=g.owner_id AND owner_scope.state='active'
    WHERE u.user_id=g.requested_by AND u.active AND (u.personal_owner_id=g.owner_id OR EXISTS (
        SELECT FROM memberships m WHERE m.user_id=u.user_id AND m.organization_id=g.owner_id AND m.active AND m.role IN ('owner','admin'))));
