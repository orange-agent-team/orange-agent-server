-- Replaying a later deletion into a quarantined older backup is neither a new
-- end-user request nor evidence that the underlying object bytes are already gone.
CREATE TABLE artifact_restore_suppressions (
    artifact_id text PRIMARY KEY,
    version_id text NOT NULL UNIQUE,
    original_owner text NOT NULL,
    content_digest text NOT NULL CHECK(content_digest ~ '^[0-9a-f]{64}$'),
    root_id text NOT NULL REFERENCES artifact_capacity(root_id),
    authority_snapshot text NOT NULL CHECK(authority_snapshot ~ '^[0-9a-f]{64}$'),
    source_deleted_at timestamptz NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_restore_suppression BEFORE UPDATE OR DELETE ON artifact_restore_suppressions
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- Facts newer than the dump also fence later replay of an absent original identity.
CREATE FUNCTION orange_guard_restored_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT FROM artifact_restore_suppressions WHERE artifact_id=NEW.artifact_id OR version_id=NEW.version_id) THEN
        RAISE EXCEPTION 'retired private object identity' USING ERRCODE='23514',CONSTRAINT='restored_version_suppressed';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER guard_restored_version BEFORE INSERT ON artifacts FOR EACH ROW EXECUTE FUNCTION orange_guard_restored_version();

CREATE TABLE artifact_restore_manifests (
    root_id text NOT NULL REFERENCES artifact_capacity(root_id),
    snapshot_digest text NOT NULL CHECK(snapshot_digest ~ '^[0-9a-f]{64}$'),
    dump_digest text NOT NULL CHECK(dump_digest ~ '^[0-9a-f]{64}$'),
    snapshot_facts bigint NOT NULL, hidden_from_backup bigint NOT NULL,
    already_deleted bigint NOT NULL, absent_from_backup bigint NOT NULL,
    CHECK(snapshot_facts=hidden_from_backup+already_deleted+absent_from_backup),
    PRIMARY KEY(root_id,snapshot_digest,dump_digest)
);
CREATE TRIGGER immutable_restore_manifest BEFORE UPDATE OR DELETE ON artifact_restore_manifests
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
