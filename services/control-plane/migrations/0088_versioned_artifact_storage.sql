-- Only a concrete, versioned provider receipt can become the source of a read.
-- Immutable pins survive removal just as the original transfer/tombstone does.
CREATE TABLE artifact_s3_versions(
    root_id text NOT NULL REFERENCES artifact_capacity(root_id),
    storage_key text NOT NULL CHECK(storage_key ~ '^(staging|published|quarantine)-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    provider_version text NOT NULL CHECK(provider_version <> 'null' AND octet_length(provider_version) BETWEEN 1 AND 1024),
    PRIMARY KEY(root_id,storage_key)
);
CREATE TRIGGER immutable_artifact_s3_version BEFORE UPDATE OR DELETE ON artifact_s3_versions
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE artifact_download_grants(
    session_id text NOT NULL REFERENCES user_sessions(session_id) ON DELETE CASCADE,
    request_key text NOT NULL,
    artifact_id text NOT NULL REFERENCES artifacts(artifact_id),
    version_id text NOT NULL REFERENCES artifacts(version_id),
    owner_id text NOT NULL REFERENCES owners(owner_id),
    requester_id text NOT NULL REFERENCES users(user_id),
    expires_unix bigint NOT NULL,
    PRIMARY KEY(session_id,request_key)
);
CREATE TRIGGER immutable_artifact_download_grant BEFORE UPDATE ON artifact_download_grants
    FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- Persist the intended root marker before touching the bucket. Recovery must
-- reuse it, even if the provider accepted the marker but its receipt was lost.
CREATE TABLE artifact_s3_bindings(
    root_id text PRIMARY KEY,
    marker_id uuid NOT NULL UNIQUE,
    provider_version text CHECK(provider_version <> 'null' AND octet_length(provider_version) BETWEEN 1 AND 1024)
);
CREATE FUNCTION protect_artifact_s3_binding() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' OR NEW.root_id<>OLD.root_id OR NEW.marker_id<>OLD.marker_id OR
       OLD.provider_version IS NOT NULL OR NEW.provider_version IS NULL THEN
        RAISE EXCEPTION 'artifact storage binding is immutable';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER protect_artifact_s3_binding BEFORE UPDATE OR DELETE ON artifact_s3_bindings
    FOR EACH ROW EXECUTE FUNCTION protect_artifact_s3_binding();
