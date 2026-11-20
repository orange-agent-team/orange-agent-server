ALTER TABLE worker_sessions ADD COLUMN transcription_input_version INTEGER NOT NULL DEFAULT 0
 CHECK(transcription_input_version IN (0,1) AND (transcription_input_version=0 OR (input_artifacts_version=1 AND execution_control_version=2)));
-- Old Gateways also fence the prior media capability when they supersede an epoch.
CREATE FUNCTION reset_transcription_capability() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.connection_epoch<>OLD.connection_epoch OR NEW.session_id<>OLD.session_id THEN
    NEW.transcription_input_version=0;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER worker_session_reset_media BEFORE UPDATE OF connection_epoch,session_id ON worker_sessions
FOR EACH ROW EXECUTE FUNCTION reset_transcription_capability();

-- A trusted ASR submission uses the ordinary input-required barrier with its separate consent.
-- Legacy gateways looking for an attached text input consequently cannot dispatch it.
CREATE FUNCTION reject_transcription_text_binding() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM jobs WHERE job_id=NEW.job_id AND purpose='transcription') THEN
    RAISE EXCEPTION 'a text descriptor cannot authorize transcription';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER job_inputs_reject_audio BEFORE INSERT ON job_inputs FOR EACH ROW EXECUTE FUNCTION reject_transcription_text_binding();

-- Cheap scheduling filter. Full current authority is also checked before offer, ACK, renewal and data delivery.
CREATE VIEW transcription_input_candidates AS
SELECT g.owner_id,g.invocation_id,g.root_budget_id,g.projection_digest,g.author_id,g.grant_id,g.observation_id,
       o.node_id,o.session_id,o.connection_epoch,o.execution_profile_id
FROM transcription_input_grants g
JOIN worker_transcription_profile_observations o USING(observation_id)
JOIN artifacts a ON a.artifact_id=g.artifact_id AND a.owner_id=g.owner_id AND a.version_id=g.version_id AND a.state='available'
JOIN nodes n ON n.node_id=o.node_id AND n.state='online' AND n.region_id=g.region_id
JOIN users u ON u.user_id=g.author_id AND u.active
WHERE g.expires_at>clock_timestamp()
AND NOT EXISTS(SELECT 1 FROM transcription_input_revocations x
               WHERE x.owner_id=g.owner_id AND x.grant_id=g.grant_id AND x.revoked_by=g.author_id);
