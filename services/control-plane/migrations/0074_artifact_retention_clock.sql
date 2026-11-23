-- Previous schemas retained publication times and output commits, but not the time
-- of every released input reference. Give legacy inputs an explicit transition date.
CREATE TABLE artifact_retention_clocks (
    artifact_id text PRIMARY KEY REFERENCES artifacts(artifact_id),
    basis text NOT NULL CHECK (basis IN ('published','last_reference_released','migration_grace')),
    anchor_at timestamptz NOT NULL
);
CREATE INDEX artifact_retention_anchor ON artifact_retention_clocks(anchor_at,artifact_id);
INSERT INTO artifact_retention_clocks(artifact_id,basis,anchor_at)
    SELECT artifact_id,'migration_grace',statement_timestamp() FROM artifacts WHERE state='available';

CREATE FUNCTION orange_start_artifact_retention() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state='available' AND (TG_OP='INSERT' OR OLD.state='staging') THEN
        INSERT INTO artifact_retention_clocks VALUES(NEW.artifact_id,'published',NEW.published_at);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER start_artifact_retention AFTER INSERT OR UPDATE OF state ON artifacts
    FOR EACH ROW EXECUTE FUNCTION orange_start_artifact_retention();

-- A release extends the clock even when another reference remains active. The last
-- release wins, transaction rollback leaves the previous clock intact.
CREATE FUNCTION orange_record_artifact_release(p_artifact text) RETURNS void LANGUAGE sql AS $$
    UPDATE artifact_retention_clocks SET basis='last_reference_released',
        anchor_at=greatest(anchor_at,clock_timestamp()) WHERE artifact_id=p_artifact;
$$;
CREATE FUNCTION orange_record_pin_release() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM orange_record_artifact_release(OLD.artifact_id);
    RETURN OLD;
END $$;
CREATE TRIGGER record_pin_release AFTER DELETE ON artifact_pins
    FOR EACH ROW EXECUTE FUNCTION orange_record_pin_release();

CREATE TRIGGER record_moved_pin_release AFTER UPDATE OF artifact_id ON artifact_pins
    FOR EACH ROW WHEN (OLD.artifact_id<>NEW.artifact_id) EXECUTE FUNCTION orange_record_pin_release();

CREATE FUNCTION orange_record_cloud_input_release() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM orange_record_artifact_release(NEW.input_artifact_id);
    RETURN NEW;
END $$;
CREATE TRIGGER record_cloud_input_release AFTER UPDATE OF status ON cloud_runs
    FOR EACH ROW WHEN (OLD.status NOT IN ('completed','failed','cancelled') AND NEW.status IN ('completed','failed','cancelled'))
    EXECUTE FUNCTION orange_record_cloud_input_release();

CREATE FUNCTION orange_record_job_input_release() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE source_id text;
BEGIN
    FOR source_id IN
        SELECT i.artifact_id FROM job_inputs i WHERE i.job_id=NEW.job_id
        UNION SELECT m.input_artifact_id FROM quote_metering m WHERE m.quote_id=NEW.quote_id
        UNION SELECT g.artifact_id FROM transcription_quote_terms q JOIN transcription_input_grants g USING(owner_id,grant_id)
            WHERE q.quote_id=NEW.quote_id ORDER BY 1
    LOOP
        PERFORM orange_record_artifact_release(source_id);
    END LOOP;
    RETURN NEW;
END $$;
CREATE TRIGGER record_job_input_release AFTER UPDATE OF execution_state ON jobs
    FOR EACH ROW WHEN (OLD.execution_state NOT IN ('succeeded','failed','cancelled') AND NEW.execution_state IN ('succeeded','failed','cancelled'))
    EXECUTE FUNCTION orange_record_job_input_release();

CREATE FUNCTION orange_artifact_active_use(p_artifact text) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS(
        SELECT FROM artifact_pins p WHERE p.artifact_id=p_artifact
            AND NOT EXISTS(SELECT FROM job_inputs i JOIN jobs j USING(job_id)
                WHERE i.artifact_id=p.artifact_id AND p.reference_id='job-input:'||i.job_id
                    AND j.execution_state IN ('succeeded','failed','cancelled'))
            AND NOT EXISTS(SELECT FROM worker_output_uploads w JOIN jobs j USING(job_id)
                WHERE w.artifact_id=p.artifact_id AND p.reference_id='job-output:'||w.job_id
                    AND j.execution_state IN ('succeeded','failed','cancelled'))
    ) OR EXISTS(SELECT FROM cloud_runs WHERE input_artifact_id=p_artifact AND status NOT IN ('completed','failed','cancelled'))
      OR EXISTS(SELECT FROM quote_metering m JOIN jobs j USING(quote_id)
        WHERE m.input_artifact_id=p_artifact AND j.execution_state NOT IN ('succeeded','failed','cancelled'))
      OR EXISTS(SELECT FROM transcription_input_grants g JOIN transcription_quote_terms q USING(owner_id,grant_id)
        JOIN jobs j ON j.quote_id=q.quote_id WHERE g.artifact_id=p_artifact AND j.execution_state NOT IN ('succeeded','failed','cancelled'));
$$;

CREATE FUNCTION orange_artifact_retention(p_artifact text)
RETURNS TABLE (basis text, anchor_at timestamptz, due_at timestamptz, protected_by_reference boolean)
LANGUAGE sql STABLE AS $$
    WITH input AS (
        SELECT a.state,a.created_at,c.basis,c.anchor_at,d.committed_at,
            EXISTS(SELECT FROM artifact_pins p WHERE p.artifact_id=a.artifact_id) OR orange_artifact_active_use(a.artifact_id) AS held
        FROM artifacts a LEFT JOIN artifact_retention_clocks c USING(artifact_id)
        LEFT JOIN job_response_commits d USING(artifact_id) WHERE a.artifact_id=p_artifact
    ), measured AS (
        SELECT CASE WHEN state='deleted' THEN 'deleted' WHEN state='staging' THEN 'staging'
                WHEN committed_at IS NOT NULL THEN 'response_commit' WHEN held THEN 'active_reference' ELSE basis END AS reason,
            CASE WHEN state='deleted' THEN NULL WHEN state='staging' THEN created_at
                WHEN committed_at IS NOT NULL THEN committed_at WHEN held THEN NULL ELSE anchor_at END AS started, held
        FROM input
    )
    SELECT reason,started,started+CASE WHEN reason='staging' THEN interval '1 hour' ELSE interval '24 hours' END,held FROM measured;
$$;
