-- Historical attempt provenance is unknown and stays NULL.
ALTER TABLE durable_events ADD COLUMN attempt_id TEXT REFERENCES worker_execution_attempts(attempt_id);

CREATE FUNCTION verify_job_event_attempt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM event_streams s
        JOIN jobs j ON j.job_id = s.entity_id AND j.owner_id = s.owner_id
        JOIN worker_execution_attempts a ON a.job_id = j.job_id
        WHERE s.stream_id = NEW.stream_id AND s.entity_kind = 'job' AND a.attempt_id = NEW.attempt_id
    ) THEN
        RAISE EXCEPTION 'attempt does not belong to the event Job and owner' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER job_event_attempt_scope BEFORE INSERT ON durable_events
FOR EACH ROW WHEN (NEW.attempt_id IS NOT NULL) EXECUTE FUNCTION verify_job_event_attempt();
