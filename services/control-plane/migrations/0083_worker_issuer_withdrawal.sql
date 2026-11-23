-- Signer identity is the public key, so a different DER encoding or reissued CA cannot revive it.
CREATE TABLE worker_issuer_policy_version (
    singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton), revision bigint NOT NULL CHECK(revision>=0)
);
INSERT INTO worker_issuer_policy_version VALUES(true,0);
CREATE TABLE worker_issuer_withdrawals (
    signing_key_sha256 text PRIMARY KEY CHECK(signing_key_sha256 ~ '^[0-9a-f]{64}$'),
    action_id text NOT NULL UNIQUE REFERENCES admin_actions(action_id),
    maker_id text NOT NULL REFERENCES admin_principals(principal_id),
    checker_id text NOT NULL REFERENCES admin_principals(principal_id) CHECK(checker_id<>maker_id),
    withdrawn_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_issuer_withdrawal BEFORE UPDATE OR DELETE ON worker_issuer_withdrawals
    FOR EACH ROW EXECUTE FUNCTION preserve_worker_issuer_observation();
CREATE TRIGGER immutable_issuer_withdrawals BEFORE TRUNCATE ON worker_issuer_withdrawals
    FOR EACH STATEMENT EXECUTE FUNCTION preserve_worker_issuer_observation();

-- One observed path per current control slot. A legacy takeover never inherits a prior session's proof.
CREATE TABLE worker_control_trust_paths (
    node_id text PRIMARY KEY REFERENCES nodes(node_id), session_id text NOT NULL UNIQUE,
    certificate_sha256 text NOT NULL REFERENCES execution_certificate_purposes(certificate_sha256),
    signing_keys text[] NOT NULL CHECK(cardinality(signing_keys) BETWEEN 1 AND 16 AND array_position(signing_keys,NULL) IS NULL)
);
CREATE FUNCTION worker_trust_path_allowed(keys text[]) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT NOT EXISTS (SELECT FROM worker_issuer_withdrawals)
        OR (COALESCE(cardinality(keys),0)>0 AND NOT EXISTS (
            SELECT FROM worker_issuer_withdrawals w WHERE w.signing_key_sha256=ANY(keys)))
$$;
CREATE FUNCTION worker_issued_leaf_allowed(issuer text) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT worker_trust_path_allowed(ARRAY(SELECT spki_sha256 FROM worker_issuer_public_keys WHERE issuer_sha256=issuer))
$$;
CREATE FUNCTION worker_control_path_allowed(node text, sid text, fingerprint text) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT worker_trust_path_allowed((SELECT signing_keys FROM worker_control_trust_paths t
        WHERE t.node_id=node AND t.session_id=sid AND t.certificate_sha256=fingerprint))
$$;
-- This short transaction fence orders withdrawal against concurrent signing, activation and control.
CREATE FUNCTION lock_worker_issuer_policy() RETURNS void LANGUAGE plpgsql VOLATILE AS $$
BEGIN
    PERFORM singleton FROM worker_issuer_policy_version WHERE singleton FOR SHARE;
END $$;
CREATE FUNCTION reject_withdrawn_issuer_write() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM lock_worker_issuer_policy();
    IF NOT worker_issued_leaf_allowed(NEW.issuer_sha256) THEN
        RAISE EXCEPTION 'device issuer is withdrawn or unverified' USING ERRCODE='OA083';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER fence_signed_device_leaf BEFORE INSERT OR UPDATE OF redemption_id ON worker_enrollment_certificates
    FOR EACH ROW EXECUTE FUNCTION reject_withdrawn_issuer_write();
CREATE FUNCTION reject_withdrawn_control_path() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.expires_at<=clock_timestamp() THEN RETURN NEW; END IF;
    PERFORM lock_worker_issuer_policy();
    IF NOT worker_control_path_allowed(NEW.node_id,NEW.session_id,NEW.certificate_sha256) THEN
        RAISE EXCEPTION 'device control path is withdrawn or unverified' USING ERRCODE='OA083';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER fence_verified_control_path BEFORE INSERT OR UPDATE OF session_id,connection_epoch,certificate_sha256,expires_at
    ON worker_sessions FOR EACH ROW EXECUTE FUNCTION reject_withdrawn_control_path();

CREATE FUNCTION validate_issuer_withdrawal_action() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(
        SELECT FROM admin_actions a JOIN admin_action_control c USING(action_id)
        WHERE a.action_id=NEW.action_id AND a.environment='dev' AND a.scope_kind='platform' AND a.scope_id='platform'
          AND a.action_type='device_issuer.withdraw' AND a.document->>'state'='ready'
          AND a.document->>'policy_version'='worker-issuer-withdraw-v1' AND a.document->>'approval_policy'='maker_checker'
          AND a.document->>'requested_by'=NEW.maker_id AND a.document#>>'{request,payload,signing_key_sha256}'=NEW.signing_key_sha256
          AND jsonb_array_length(a.document->'approvals')=1 AND a.document#>>'{approvals,0,approver_id}'=NEW.checker_id
          AND a.document#>>'{approvals,0,decision}'='approve' AND a.document#>>'{approvals,0,payload_digest}'=a.document->>'payload_digest'
          AND a.document#>>'{approvals,0,policy_version}'=a.document->>'policy_version'
          AND c.expires_at>floor(extract(epoch FROM clock_timestamp()))::bigint
    ) THEN RAISE EXCEPTION 'verified issuer withdrawal approval required' USING ERRCODE='23514'; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER approved_issuer_withdrawal BEFORE INSERT ON worker_issuer_withdrawals
    FOR EACH ROW EXECUTE FUNCTION validate_issuer_withdrawal_action();
CREATE FUNCTION advance_worker_issuer_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND NEW.singleton AND OLD.singleton AND NEW.revision=OLD.revision+1
       AND NEW.revision=(SELECT count(*) FROM worker_issuer_withdrawals) THEN RETURN NEW; END IF;
    RAISE EXCEPTION 'invalid signer policy revision' USING ERRCODE='23514';
END $$;
CREATE TRIGGER monotonic_worker_issuer_revision BEFORE UPDATE OR DELETE ON worker_issuer_policy_version
    FOR EACH ROW EXECUTE FUNCTION advance_worker_issuer_revision();
CREATE TRIGGER retain_worker_issuer_revision BEFORE TRUNCATE ON worker_issuer_policy_version
    FOR EACH STATEMENT EXECUTE FUNCTION preserve_worker_issuer_observation();
