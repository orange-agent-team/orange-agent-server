-- Private immutable input objects; Job/Worker delivery remains a separate gate.
CREATE TABLE artifact_capacity(
 root_id TEXT PRIMARY KEY,byte_limit BIGINT NOT NULL CHECK(byte_limit>0),
 record_limit BIGINT NOT NULL CHECK(record_limit>0),used_bytes BIGINT NOT NULL DEFAULT 0 CHECK(used_bytes>=0),
 records BIGINT NOT NULL DEFAULT 0 CHECK(records>=0),CHECK(used_bytes<=byte_limit),CHECK(records<=record_limit)
);
CREATE TABLE artifacts(
 artifact_id TEXT PRIMARY KEY,version_id TEXT NOT NULL UNIQUE,
 root_id TEXT NOT NULL REFERENCES artifact_capacity(root_id),owner_id TEXT NOT NULL REFERENCES owners(owner_id),
 creator_id TEXT NOT NULL REFERENCES users(user_id),resource_id TEXT UNIQUE REFERENCES team_resources(resource_id),
 sha256 TEXT NOT NULL CHECK(sha256 ~ '^[0-9a-f]{64}$'),size_bytes BIGINT NOT NULL CHECK(size_bytes BETWEEN 0 AND 104857600),
 media_type TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'staging' CHECK(state IN('staging','available','deleted')),
 published_transfer_id TEXT UNIQUE,created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),published_at TIMESTAMPTZ,deleted_at TIMESTAMPTZ,
 CHECK((state='available') IS NOT TRUE OR (published_transfer_id IS NOT NULL AND published_at IS NOT NULL)),
 CHECK((state='deleted')=(deleted_at IS NOT NULL))
);
CREATE TABLE artifact_uploads(
 owner_id TEXT NOT NULL REFERENCES owners(owner_id),upload_id TEXT NOT NULL,artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(artifact_id),
 request_digest TEXT NOT NULL,PRIMARY KEY(owner_id,upload_id)
);
CREATE TABLE artifact_transfers(
 transfer_id TEXT PRIMARY KEY,artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
 stage_key TEXT NOT NULL UNIQUE,published_key TEXT NOT NULL UNIQUE,
 reserved_bytes BIGINT NOT NULL CHECK(reserved_bytes>=0),
 state TEXT NOT NULL CHECK(state IN('writing','orphan','attached','purging','purged')),
 stage_deleted BOOLEAN NOT NULL DEFAULT FALSE,published_deleted BOOLEAN NOT NULL DEFAULT FALSE,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),expires_at TIMESTAMPTZ NOT NULL,finished_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX artifact_one_writer ON artifact_transfers(artifact_id) WHERE state='writing';
ALTER TABLE artifacts ADD CONSTRAINT artifact_published_transfer_fk FOREIGN KEY(published_transfer_id) REFERENCES artifact_transfers(transfer_id);
CREATE TABLE artifact_pins(
 artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),reference_id TEXT NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),PRIMARY KEY(artifact_id,reference_id)
);
CREATE TABLE artifact_tombstones(
 artifact_id TEXT PRIMARY KEY REFERENCES artifacts(artifact_id),actor_id TEXT NOT NULL REFERENCES users(user_id),
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER artifact_tombstone_immutable BEFORE UPDATE OR DELETE ON artifact_tombstones FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION protect_artifact_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'artifact identity history is immutable'; END IF;
 IF ROW(NEW.artifact_id,NEW.version_id,NEW.root_id,NEW.owner_id,NEW.creator_id,NEW.resource_id,NEW.sha256,NEW.size_bytes,NEW.media_type)
 IS DISTINCT FROM ROW(OLD.artifact_id,OLD.version_id,OLD.root_id,OLD.owner_id,OLD.creator_id,OLD.resource_id,OLD.sha256,OLD.size_bytes,OLD.media_type)
 OR (OLD.published_transfer_id IS NOT NULL AND NEW.published_transfer_id IS DISTINCT FROM OLD.published_transfer_id)
 OR (OLD.state='deleted' AND NEW.state!='deleted')
 THEN RAISE EXCEPTION 'artifact content/version is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER artifact_identity_immutable BEFORE UPDATE OR DELETE ON artifacts FOR EACH ROW EXECUTE FUNCTION protect_artifact_identity();
CREATE TRIGGER artifact_upload_immutable BEFORE UPDATE OR DELETE ON artifact_uploads FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
