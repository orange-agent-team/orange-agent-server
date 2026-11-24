-- Reserve version names across the pre-existing trusted profile provisioner and the new registry.
-- Historical profiles retain exactly the identity and enabled state they had before this migration.
CREATE TABLE model_version_names (
    model_version_id text PRIMARY KEY,
    authority text NOT NULL CHECK(authority IN ('execution_profile','manifest'))
);
LOCK TABLE reviewed_execution_profiles IN SHARE ROW EXCLUSIVE MODE;
INSERT INTO model_version_names
    SELECT DISTINCT model_version_id,'execution_profile' FROM reviewed_execution_profiles;

CREATE FUNCTION keep_model_name() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND NEW IS NOT DISTINCT FROM OLD THEN RETURN NEW; END IF;
    RAISE EXCEPTION 'model version namespace is immutable' USING ERRCODE='23514';
END $$;
CREATE TRIGGER keep_model_name BEFORE UPDATE OR DELETE ON model_version_names
FOR EACH ROW EXECUTE FUNCTION keep_model_name();
CREATE TRIGGER keep_all_model_names BEFORE TRUNCATE ON model_version_names
FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();

CREATE FUNCTION reserve_model_name(version_id text, requested text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO model_version_names AS n(model_version_id,authority) VALUES(version_id,requested)
      ON CONFLICT (model_version_id) DO UPDATE SET model_version_id=excluded.model_version_id
      WHERE n.authority=excluded.authority;
    IF NOT FOUND THEN RAISE EXCEPTION 'model version already belongs to another authority'
        USING ERRCODE='23514',CONSTRAINT='model_version_namespace_conflict'; END IF;
END $$;
CREATE FUNCTION reserve_execution_model() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM reserve_model_name(NEW.model_version_id,'execution_profile');
    RETURN NEW;
END $$;
CREATE TRIGGER reserve_execution_model BEFORE INSERT ON reviewed_execution_profiles
FOR EACH ROW EXECUTE FUNCTION reserve_execution_model();

CREATE TABLE admin_model_drafts (
    model_version_id text PRIMARY KEY REFERENCES model_version_names,
    manifest_sha256 text NOT NULL UNIQUE CHECK(manifest_sha256 ~ '^[0-9a-f]{64}$'),
    manifest jsonb NOT NULL CHECK(jsonb_typeof(manifest)='object'
        AND manifest ? 'model_version' AND manifest->>'model_version'=model_version_id
        AND manifest#>>'{interface,modality}' IS NOT NULL AND manifest#>>'{interface,modality}' IN ('text','embedding','image','transcription')),
    created_by text NOT NULL REFERENCES admin_principals(principal_id),
    request_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(created_by,request_key)
);
CREATE FUNCTION reserve_manifest_model() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM reserve_model_name(NEW.model_version_id,'manifest');
    RETURN NEW;
END $$;
CREATE TRIGGER reserve_manifest_name BEFORE INSERT ON admin_model_drafts
FOR EACH ROW EXECUTE FUNCTION reserve_manifest_model();
CREATE TRIGGER keep_model_manifest BEFORE UPDATE OR DELETE ON admin_model_drafts
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_all_model_manifests BEFORE TRUNCATE ON admin_model_drafts
FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX admin_model_drafts_page ON admin_model_drafts(model_version_id COLLATE "C");
