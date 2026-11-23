CREATE TABLE artifact_retention_expirations (
    artifact_id text PRIMARY KEY REFERENCES artifacts(artifact_id),
    version_id text NOT NULL REFERENCES artifacts(version_id),
    basis text NOT NULL CHECK(basis IN ('published','migration_grace','last_reference_released','response_commit')),
    due_at timestamptz NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK(observed_at>=due_at),
    FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id)
);
CREATE TRIGGER retention_expiration_immutable BEFORE UPDATE OR DELETE ON artifact_retention_expirations
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE INDEX artifact_retention_cloud_active ON cloud_runs(input_artifact_id) WHERE status NOT IN ('completed','failed','cancelled');
CREATE INDEX artifact_retention_text_input ON quote_metering(input_artifact_id,quote_id);
CREATE INDEX artifact_retention_asr_input ON transcription_input_grants(artifact_id,owner_id,grant_id);
CREATE INDEX artifact_retention_asr_quotes ON transcription_quote_terms(owner_id,grant_id,quote_id);
CREATE INDEX artifact_retention_output_time ON job_response_commits(committed_at,artifact_id);

-- Look up the individual version's clock for content access. No implicit retention
-- extension occurs for reads, retries, status requests or creation of a quote.
CREATE FUNCTION orange_artifact_content_live(p_artifact text) RETURNS boolean LANGUAGE sql VOLATILE AS $$
    SELECT EXISTS(SELECT FROM orange_artifact_retention(p_artifact) r
        WHERE r.basis='active_reference' OR (
            r.basis IN ('published','migration_grace','last_reference_released','response_commit')
            AND r.due_at>clock_timestamp()));
$$;

CREATE OR REPLACE FUNCTION protect_artifact_pin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM artifact_id FROM artifacts WHERE artifact_id=NEW.artifact_id AND state='available' FOR SHARE;
    IF NOT FOUND OR NOT orange_artifact_content_live(NEW.artifact_id) THEN
        RAISE EXCEPTION 'active artifact content required' USING ERRCODE='23514',CONSTRAINT='original_artifact_retention';
    END IF;
    RETURN NEW;
END $$;

-- Durable active references may start only while their original input is still accessible.
-- This also protects older writers sharing the migrated database.
CREATE FUNCTION orange_guard_original_retention() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE source_id text;
BEGIN
    IF TG_TABLE_NAME='cloud_runs' THEN
        source_id:=NEW.input_artifact_id;
    ELSE
        SELECT m.input_artifact_id INTO source_id FROM quote_metering m WHERE m.quote_id=NEW.quote_id;
        IF NOT FOUND THEN
            SELECT g.artifact_id INTO source_id FROM transcription_quote_terms q JOIN transcription_input_grants g USING(owner_id,grant_id)
            WHERE q.quote_id=NEW.quote_id;
        END IF;
    END IF;
    IF source_id IS NOT NULL THEN
        PERFORM artifact_id FROM artifacts WHERE artifact_id=source_id AND state='available' FOR SHARE;
        IF NOT FOUND OR NOT orange_artifact_content_live(source_id) THEN
            RAISE EXCEPTION 'active original input required' USING ERRCODE='23514',CONSTRAINT='original_artifact_retention';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER guard_cloud_input_retention BEFORE INSERT ON cloud_runs FOR EACH ROW EXECUTE FUNCTION orange_guard_original_retention();
CREATE TRIGGER guard_job_input_retention BEFORE INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION orange_guard_original_retention();

-- A user's earlier withdrawal can hide a version before the normal policy
-- deadline. Keep the earlier of its standard deadlines when reclaiming bytes.
CREATE FUNCTION orange_deleted_version_due(p_artifact text) RETURNS boolean LANGUAGE sql VOLATILE AS $$
    SELECT EXISTS(SELECT FROM artifacts a
        LEFT JOIN artifact_retention_clocks c USING(artifact_id)
        LEFT JOIN job_response_commits d USING(artifact_id)
        WHERE a.artifact_id=p_artifact AND a.state='deleted'
          AND (a.deleted_at+interval '24 hours'<=clock_timestamp()
               OR coalesce(d.committed_at,c.anchor_at)+interval '24 hours'<=clock_timestamp()
               OR EXISTS(SELECT FROM artifact_retention_expirations e WHERE e.artifact_id=p_artifact))
          AND NOT EXISTS(SELECT FROM artifact_pins p WHERE p.artifact_id=p_artifact)
          AND NOT orange_artifact_active_use(p_artifact));
$$;
