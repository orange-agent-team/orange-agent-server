-- The driver creates the temporary table and streams the verified data inside this transaction.
DO $$
BEGIN
    IF NOT EXISTS(SELECT FROM artifact_capacity WHERE root_id=current_setting('orange.restore_root')) THEN
        RAISE EXCEPTION 'restore root mismatch';
    END IF;
    IF EXISTS(SELECT FROM restore_facts f JOIN artifacts a USING(artifact_id)
        WHERE a.root_id<>current_setting('orange.restore_root') OR a.version_id<>f.version_id
           OR a.owner_id<>f.owner_id OR a.sha256<>f.content_digest)
       OR EXISTS(SELECT FROM restore_facts f JOIN artifacts a USING(version_id) WHERE a.artifact_id<>f.artifact_id)
       OR EXISTS(SELECT FROM restore_facts f JOIN artifact_restore_suppressions r
            ON r.artifact_id=f.artifact_id OR r.version_id=f.version_id
            WHERE (r.root_id,r.artifact_id,r.version_id,r.original_owner,r.content_digest,r.source_deleted_at)
              IS DISTINCT FROM (current_setting('orange.restore_root'),f.artifact_id,f.version_id,f.owner_id,f.content_digest,f.deleted_at)) THEN
        RAISE EXCEPTION 'restored version identity disagrees with retirement evidence';
    END IF;
END $$;

-- A newer tombstone can refer to an object created after this backup. Such an object is already absent.
CREATE TEMP TABLE replay_changes ON COMMIT DROP AS
    SELECT a.artifact_id,a.resource_id,a.version_id,a.root_id,f.deleted_at
    FROM restore_facts f JOIN artifacts a USING(artifact_id) WHERE a.state<>'deleted';
INSERT INTO artifact_restore_suppressions(artifact_id,version_id,root_id,original_owner,content_digest,authority_snapshot,source_deleted_at)
    SELECT f.artifact_id,f.version_id,current_setting('orange.restore_root'),f.owner_id,f.content_digest,current_setting('orange.restore_hash'),f.deleted_at
    FROM restore_facts f ON CONFLICT(artifact_id) DO NOTHING;
UPDATE artifacts a SET state='deleted',deleted_at=c.deleted_at FROM replay_changes c WHERE a.artifact_id=c.artifact_id;
UPDATE team_resources r SET deleted=true,acl_version=acl_version+1 FROM replay_changes c WHERE r.resource_id=c.resource_id AND NOT r.deleted;
UPDATE artifact_capacity SET records=records-(SELECT count(*) FROM replay_changes) WHERE root_id=current_setting('orange.restore_root');
DO $$
BEGIN
    IF EXISTS(SELECT FROM restore_facts f JOIN artifacts a USING(artifact_id) WHERE a.state<>'deleted') THEN
        RAISE EXCEPTION 'restored version still accessible';
    END IF;
END $$;
INSERT INTO artifact_restore_manifests
    SELECT current_setting('orange.restore_root'),current_setting('orange.restore_hash'),current_setting('orange.restore_dump'),
           (SELECT count(*) FROM restore_facts),(SELECT count(*) FROM replay_changes),
           (SELECT count(*) FROM restore_facts f JOIN artifacts a USING(artifact_id))-(SELECT count(*) FROM replay_changes),
           (SELECT count(*) FROM restore_facts f WHERE NOT EXISTS(SELECT FROM artifacts a WHERE a.artifact_id=f.artifact_id))
    ON CONFLICT DO NOTHING;
SELECT json_build_object('snapshot_facts',snapshot_facts,'hidden_from_backup',hidden_from_backup,'already_deleted',already_deleted,'absent_from_backup',absent_from_backup)
    FROM artifact_restore_manifests WHERE root_id=current_setting('orange.restore_root') AND snapshot_digest=current_setting('orange.restore_hash')
    AND dump_digest=current_setting('orange.restore_dump');
COMMIT;
