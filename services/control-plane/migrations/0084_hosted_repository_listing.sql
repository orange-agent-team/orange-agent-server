-- A model tool selection is not a grant. Only a later approved operation may use
-- an unrevoked Run grant belonging to the selected repository provider.
ALTER TABLE cloud_connector_plans DROP CONSTRAINT cloud_connector_plans_check;
ALTER TABLE cloud_connector_plans ADD CONSTRAINT cloud_connector_plans_check CHECK (
    (connector_kind='postgres' AND tool_name IN ('list_views','describe_view','query_view'))
 OR (connector_kind='s3' AND tool_name IN ('list_objects','head_object','read_object'))
 OR (connector_kind='github' AND tool_name='list_repositories')
 OR (connector_kind='gitlab' AND tool_name='list_projects')
);

CREATE OR REPLACE VIEW cloud_connector_retention AS
WITH live_grants AS (
    SELECT grant_id,run_id,'postgres'::text AS provider,expires_at,source_digest FROM pg_view_grants g
    WHERE NOT EXISTS(SELECT FROM pg_view_revocations v WHERE v.grant_id=g.grant_id)
    UNION ALL
    SELECT grant_id,run_id,'s3',expires_at,source_digest FROM s3_source_grants g
    WHERE NOT EXISTS(SELECT FROM s3_source_revocations v WHERE v.grant_id=g.grant_id)
    UNION ALL
    SELECT grant_id,run_id,provider,expires_at,source_digest FROM repository_source_grants g
    WHERE provider IN ('github','gitlab') AND NOT EXISTS(SELECT FROM repository_source_revocations v WHERE v.grant_id=g.grant_id)
)
SELECT d.run_id,g.expires_at AS retain_until
FROM cloud_connector_decisions d JOIN cloud_connector_plans p USING(run_id)
JOIN cloud_connector_model_commits m USING(run_id) JOIN cloud_runs r USING(run_id)
JOIN live_grants g ON g.run_id=d.run_id AND g.grant_id=d.grant_id AND g.provider=p.connector_kind
JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id
JOIN artifacts response ON response.artifact_id=m.artifact_id AND response.version_id=m.version_id
WHERE a.state='available' AND response.state='available' AND r.status IN ('waiting_approval','paused','running')
AND g.expires_at>clock_timestamp()
AND NOT EXISTS(SELECT FROM connector_credential_faults f WHERE f.source_digest=g.source_digest);

CREATE FUNCTION purge_model_repository_content() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME='repository_source_revocations' THEN
        DELETE FROM cloud_connector_result_content c USING cloud_connector_decisions d,cloud_connector_plans p,repository_source_grants g
        WHERE c.run_id=d.run_id AND p.run_id=d.run_id AND g.run_id=d.run_id AND g.provider=p.connector_kind
          AND g.grant_id=d.grant_id AND g.grant_id=NEW.grant_id;
    ELSIF NEW.connector_kind IN ('github','gitlab') THEN
        DELETE FROM cloud_connector_result_content c USING cloud_connector_decisions d,cloud_connector_plans p,repository_source_grants g
        WHERE c.run_id=d.run_id AND p.run_id=d.run_id AND g.run_id=d.run_id AND g.provider=p.connector_kind
          AND g.grant_id=d.grant_id AND g.source_digest=NEW.source_digest AND g.provider=NEW.connector_kind;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER purge_repository_model_content AFTER INSERT ON repository_source_revocations
    FOR EACH ROW EXECUTE FUNCTION purge_model_repository_content();
CREATE TRIGGER purge_refused_repository_model_content AFTER INSERT ON connector_credential_faults
    FOR EACH ROW EXECUTE FUNCTION purge_model_repository_content();
