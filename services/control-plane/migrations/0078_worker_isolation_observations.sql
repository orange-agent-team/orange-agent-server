CREATE TABLE worker_isolation_observations(
    observation_id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id text NOT NULL REFERENCES nodes(node_id),
    session_id text NOT NULL,
    connection_epoch bigint NOT NULL CHECK(connection_epoch>0),
    node_policy_generation bigint NOT NULL CHECK(node_policy_generation>=0),
    request_id text NOT NULL,
    certificate_sha256 text NOT NULL REFERENCES worker_credentials(certificate_sha256),
    capabilities_digest text NOT NULL CHECK(capabilities_digest ~ '^[0-9a-f]{64}$'),
    profile_id text NOT NULL CHECK(profile_id='wsl2-reviewed-text-v1'),
    policy_digest text NOT NULL CHECK(policy_digest ~ '^[0-9a-f]{64}$'),
    probe_digest text NOT NULL CHECK(probe_digest ~ '^[0-9a-f]{64}$'),
    observation_digest text NOT NULL CHECK(observation_digest ~ '^[0-9a-f]{64}$'),
    process_observation jsonb NOT NULL,
    disposition text NOT NULL CHECK(disposition IN ('incomplete','independent_review_required')),
    reported_capture_unix bigint NOT NULL CHECK(reported_capture_unix>0),
    received_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(node_id,session_id,connection_epoch,request_id),
    CHECK(process_observation @> '{"protocol":1,"runtime_verified":false,"supply_authorized":false}'::jsonb)
);
CREATE INDEX isolation_observation_node_time ON worker_isolation_observations(node_id,received_at);
CREATE INDEX isolation_observation_retention ON worker_isolation_observations(received_at,observation_id);
CREATE FUNCTION protect_isolation_observation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' AND OLD.received_at<=clock_timestamp()-interval '30 days' THEN RETURN OLD; END IF;
    RAISE EXCEPTION 'isolation observation is immutable during retention' USING ERRCODE='23514';
END $$;
CREATE TRIGGER immutable_isolation_observation BEFORE UPDATE OR DELETE ON worker_isolation_observations
    FOR EACH ROW EXECUTE FUNCTION protect_isolation_observation();

-- A historical receipt stays immutable when the control session or policy changes.
-- Project only the latest bounded diagnostic metadata for live platform inspection.
CREATE VIEW current_worker_isolation_observations AS
SELECT DISTINCT ON (e.node_id) e.node_id,e.observation_id,e.session_id,e.connection_epoch,
    e.node_policy_generation,e.capabilities_digest,e.profile_id,e.policy_digest,e.probe_digest,
    e.disposition,e.reported_capture_unix,e.received_at,false AS supply_authorized,false AS wasm_enabled
FROM worker_isolation_observations e
JOIN worker_sessions s USING(node_id,session_id,connection_epoch,capabilities_digest,certificate_sha256)
JOIN nodes n ON n.node_id=e.node_id AND n.policy_generation=e.node_policy_generation
JOIN worker_credentials c ON c.node_id=e.node_id AND c.certificate_sha256=e.certificate_sha256
JOIN owners o ON o.owner_id=n.owner_id
JOIN users u ON u.user_id=c.enrollment_user_id
WHERE s.expires_at>statement_timestamp() AND n.state='online' AND o.state='active' AND u.active
    AND c.active AND c.not_before<=statement_timestamp() AND c.expires_at>statement_timestamp()
    AND e.reported_capture_unix BETWEEN floor(extract(epoch FROM statement_timestamp()))::bigint-300
                                    AND floor(extract(epoch FROM statement_timestamp()))::bigint+30
    AND (n.owner_id=u.personal_owner_id OR EXISTS (
        SELECT FROM memberships m WHERE m.organization_id=n.owner_id AND m.user_id=u.user_id
            AND m.active AND m.role IN ('owner','admin')))
ORDER BY e.node_id,e.reported_capture_unix DESC,e.received_at DESC,e.observation_id DESC;
