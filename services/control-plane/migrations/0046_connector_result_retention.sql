-- Keep evidence immutable while allowing the source payload to expire or be erased.
ALTER TABLE cloud_connector_effect_receipts ADD UNIQUE(run_id,result_digest);
CREATE TABLE cloud_connector_result_content (
    run_id text PRIMARY KEY,
    digest text NOT NULL,
    result jsonb NOT NULL CHECK (octet_length(result::text)<=3145728),
    retain_until timestamptz NOT NULL,
    FOREIGN KEY(run_id,digest) REFERENCES cloud_connector_effect_receipts(run_id,result_digest)
);
CREATE INDEX cloud_connector_result_expiry ON cloud_connector_result_content(retain_until);
CREATE VIEW cloud_connector_retention AS
SELECT d.run_id, COALESCE(pg.expires_at,s3.expires_at) AS retain_until
FROM cloud_connector_decisions d JOIN cloud_connector_plans p USING(run_id)
JOIN cloud_connector_model_commits m USING(run_id) JOIN cloud_runs r USING(run_id)
JOIN artifacts a ON a.artifact_id=r.input_artifact_id AND a.version_id=r.input_version_id
JOIN artifacts response ON response.artifact_id=m.artifact_id AND response.version_id=m.version_id
LEFT JOIN pg_view_grants pg ON p.connector_kind='postgres' AND pg.grant_id=d.grant_id AND pg.run_id=d.run_id
LEFT JOIN s3_source_grants s3 ON p.connector_kind='s3' AND s3.grant_id=d.grant_id AND s3.run_id=d.run_id
WHERE a.state='available' AND response.state='available' AND r.status IN ('waiting_approval','paused','running')
AND COALESCE(pg.expires_at,s3.expires_at)>clock_timestamp()
AND NOT EXISTS(SELECT FROM pg_view_revocations v WHERE v.grant_id=pg.grant_id)
AND NOT EXISTS(SELECT FROM s3_source_revocations v WHERE v.grant_id=s3.grant_id)
AND NOT EXISTS(SELECT FROM connector_credential_faults f WHERE f.source_digest=COALESCE(pg.source_digest,s3.source_digest));
INSERT INTO cloud_connector_result_content(run_id,digest,result,retain_until)
SELECT r.run_id,r.result_digest,r.result,a.retain_until FROM cloud_connector_effect_receipts r
JOIN cloud_connector_retention a USING(run_id) WHERE r.state='succeeded' AND r.result IS NOT NULL;
ALTER TABLE cloud_connector_effect_receipts DROP COLUMN result;

CREATE FUNCTION purge_connector_grant_content() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM cloud_connector_result_content c USING cloud_connector_decisions d,cloud_connector_plans p
    WHERE c.run_id=d.run_id AND p.run_id=d.run_id AND p.connector_kind=TG_ARGV[0] AND d.grant_id=NEW.grant_id;
    RETURN NEW;
END $$;
CREATE TRIGGER purge_pg_model_content AFTER INSERT ON pg_view_revocations
FOR EACH ROW EXECUTE FUNCTION purge_connector_grant_content('postgres');
CREATE TRIGGER purge_s3_model_content AFTER INSERT ON s3_source_revocations
FOR EACH ROW EXECUTE FUNCTION purge_connector_grant_content('s3');

CREATE FUNCTION purge_connector_source_content() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM cloud_connector_result_content c USING cloud_connector_decisions d,cloud_connector_plans p
    WHERE c.run_id=d.run_id AND p.run_id=d.run_id AND p.connector_kind=NEW.connector_kind AND
    ((NEW.connector_kind='postgres' AND EXISTS(SELECT FROM pg_view_grants g WHERE g.grant_id=d.grant_id AND g.source_digest=NEW.source_digest))
    OR (NEW.connector_kind='s3' AND EXISTS(SELECT FROM s3_source_grants g WHERE g.grant_id=d.grant_id AND g.source_digest=NEW.source_digest)));
    RETURN NEW;
END $$;
CREATE TRIGGER purge_refused_model_source AFTER INSERT ON connector_credential_faults
FOR EACH ROW EXECUTE FUNCTION purge_connector_source_content();
CREATE FUNCTION purge_connector_artifact_content() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM cloud_connector_result_content c USING cloud_runs r,cloud_connector_model_commits m
    WHERE c.run_id=r.run_id AND m.run_id=r.run_id AND NEW.artifact_id IN (r.input_artifact_id,m.artifact_id);
    RETURN NEW;
END $$;
CREATE TRIGGER purge_model_connector_input AFTER INSERT ON artifact_tombstones
FOR EACH ROW EXECUTE FUNCTION purge_connector_artifact_content();
