-- The registry stores signed evidence separately from approval. Uploading evidence grants no execution.
ALTER TABLE admin_model_drafts ADD UNIQUE(model_version_id,manifest_sha256);
CREATE TABLE model_certifications(
 report_id text PRIMARY KEY CHECK(report_id ~ '^[0-9a-f]{64}$'),
 model_version_id text NOT NULL REFERENCES admin_model_drafts(model_version_id),
 manifest_sha256 text NOT NULL REFERENCES admin_model_drafts(manifest_sha256),
 signing_key_sha256 text NOT NULL CHECK(signing_key_sha256 ~ '^[0-9a-f]{64}$'),
 model_sha256 text NOT NULL CHECK(model_sha256 ~ '^[0-9a-f]{64}$'),
 execution_profile_id text NOT NULL,
 execution_profile_digest text NOT NULL CHECK(execution_profile_digest ~ '^[0-9a-f]{64}$'),
 issued_unix bigint NOT NULL, expires_unix bigint NOT NULL CHECK(expires_unix>issued_unix),
 envelope jsonb NOT NULL,
 registered_by text NOT NULL REFERENCES admin_principals(principal_id),
 request_key text NOT NULL, UNIQUE(registered_by,request_key),
 FOREIGN KEY(model_version_id,manifest_sha256) REFERENCES admin_model_drafts(model_version_id,manifest_sha256),
 UNIQUE(model_version_id,report_id)
);
CREATE INDEX model_certifications_version ON model_certifications(model_version_id,report_id);
CREATE TABLE model_registry_activations(
 model_version_id text PRIMARY KEY REFERENCES admin_model_drafts(model_version_id),
 report_id text NOT NULL UNIQUE,
 action_id text NOT NULL UNIQUE REFERENCES admin_actions(action_id),
 maker_id text NOT NULL REFERENCES admin_principals(principal_id),
 checker_id text NOT NULL REFERENCES admin_principals(principal_id),
 activated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK(maker_id<>checker_id),
 FOREIGN KEY(model_version_id,report_id) REFERENCES model_certifications(model_version_id,report_id)
);
CREATE TABLE model_registry_disables(
 model_version_id text PRIMARY KEY REFERENCES admin_model_drafts(model_version_id),
 action_id text NOT NULL UNIQUE REFERENCES admin_actions(action_id),
 actor_id text NOT NULL REFERENCES admin_principals(principal_id),
 disabled_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER keep_model_certifications BEFORE UPDATE OR DELETE ON model_certifications FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_all_model_certifications BEFORE TRUNCATE ON model_certifications FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_model_activation BEFORE UPDATE OR DELETE ON model_registry_activations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_all_model_activations BEFORE TRUNCATE ON model_registry_activations FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_model_disables BEFORE UPDATE OR DELETE ON model_registry_disables FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_all_model_disables BEFORE TRUNCATE ON model_registry_disables FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();

-- A single row serializes new certification effects, provider admission and withdrawal for one version.
CREATE FUNCTION guard_model_decision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM model_version_id FROM model_version_names WHERE model_version_id=NEW.model_version_id AND authority='manifest' FOR UPDATE;
    IF NOT FOUND OR EXISTS(SELECT FROM model_registry_disables WHERE model_version_id=NEW.model_version_id) THEN
        RAISE EXCEPTION 'unknown or withdrawn model' USING ERRCODE='23514'; END IF;
    IF NOT EXISTS(SELECT FROM admin_actions a WHERE a.action_id=NEW.action_id AND a.environment='dev'
        AND a.scope_kind='platform' AND a.scope_id='platform' AND a.document->>'state'='ready'
        AND a.document#>>'{request,payload,model_version_id}'=NEW.model_version_id
        AND a.action_type=CASE WHEN TG_TABLE_NAME='model_registry_activations' THEN 'model.activate' ELSE 'model.disable' END)
    THEN RAISE EXCEPTION 'missing approved model action' USING ERRCODE='23514'; END IF;
    IF TG_TABLE_NAME='model_registry_activations' THEN
      IF NOT EXISTS(SELECT FROM admin_actions a WHERE a.action_id=NEW.action_id
          AND a.document->>'requested_by'=NEW.maker_id AND a.document#>>'{approvals,0,approver_id}'=NEW.checker_id
          AND a.document#>>'{request,payload,certification_report_id}'=NEW.report_id)
      THEN RAISE EXCEPTION 'model approval identity mismatch' USING ERRCODE='23514'; END IF;
      IF NOT EXISTS(
        SELECT FROM model_certifications c JOIN admin_model_drafts d USING(model_version_id,manifest_sha256)
          WHERE c.model_version_id=NEW.model_version_id AND c.report_id=NEW.report_id
          AND c.issued_unix<=extract(epoch FROM clock_timestamp()) AND c.expires_unix>extract(epoch FROM clock_timestamp())) THEN
        RAISE EXCEPTION 'invalid or expired certification' USING ERRCODE='23514'; END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER check_model_activation BEFORE INSERT ON model_registry_activations FOR EACH ROW EXECUTE FUNCTION guard_model_decision();
CREATE TRIGGER check_model_disable BEFORE INSERT ON model_registry_disables FOR EACH ROW EXECUTE FUNCTION guard_model_decision();

CREATE VIEW registry_model_states AS
SELECT d.model_version_id,d.manifest_sha256,d.manifest,d.created_by,d.request_key,
 d.manifest#>>'{interface,modality}' AS modality,
 CASE WHEN x.action_id IS NOT NULL OR c.expires_unix<=extract(epoch FROM clock_timestamp()) THEN 'disabled' WHEN a.action_id IS NOT NULL THEN 'active' ELSE 'draft' END AS status,
 coalesce(x.action_id,CASE WHEN c.expires_unix<=extract(epoch FROM clock_timestamp()) THEN 'expired-'||a.action_id ELSE a.action_id END,d.manifest_sha256) AS version,a.report_id AS certification_report_id
FROM admin_model_drafts d LEFT JOIN model_registry_activations a USING(model_version_id)
LEFT JOIN model_registry_disables x USING(model_version_id) LEFT JOIN model_certifications c ON c.report_id=a.report_id;

-- Preserve the exact trusted profile identities present at rollout. Ordinary application roles
-- have no write grant on this migration inventory; current profile INSERT alone is never an approval.
LOCK TABLE reviewed_execution_profiles IN SHARE ROW EXCLUSIVE MODE;
CREATE TABLE model_legacy_profile_pins (
    execution_profile_id text PRIMARY KEY, model_version_id text NOT NULL, model_sha256 text NOT NULL,
    execution_profile_digest text NOT NULL, enabled_at_upgrade boolean NOT NULL DEFAULT true
);
INSERT INTO model_legacy_profile_pins
    SELECT execution_profile_id,model_version_id,model_sha256,execution_profile_digest,enabled FROM reviewed_execution_profiles;
REVOKE ALL ON model_legacy_profile_pins FROM PUBLIC;
GRANT SELECT ON model_legacy_profile_pins TO PUBLIC;
CREATE TRIGGER keep_model_legacy_profile_pins BEFORE UPDATE OR DELETE ON model_legacy_profile_pins FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER keep_all_model_legacy_profile_pins BEFORE TRUNCATE ON model_legacy_profile_pins FOR EACH STATEMENT EXECUTE FUNCTION forbid_ledger_mutation();
CREATE OR REPLACE FUNCTION reserve_execution_model() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE claimed text;
BEGIN
    IF EXISTS(SELECT FROM model_legacy_profile_pins p WHERE p.execution_profile_id=NEW.execution_profile_id AND p.model_version_id=NEW.model_version_id
       AND p.model_sha256=NEW.model_sha256 AND p.execution_profile_digest=NEW.execution_profile_digest AND (NOT NEW.enabled OR p.enabled_at_upgrade)) THEN
        PERFORM reserve_model_name(NEW.model_version_id,'execution_profile');
        RETURN NEW;
    END IF;
    SELECT authority INTO claimed FROM model_version_names WHERE model_version_id=NEW.model_version_id FOR UPDATE;
    IF claimed IS DISTINCT FROM 'manifest' THEN
        RAISE EXCEPTION 'model version has no reviewed provenance' USING ERRCODE='23514',CONSTRAINT='model_version_namespace_conflict';
    END IF;
    IF (TG_OP='INSERT' OR NEW.enabled) AND NOT EXISTS(
        SELECT FROM model_registry_activations a JOIN model_certifications c USING(model_version_id,report_id)
          WHERE a.model_version_id=NEW.model_version_id AND c.execution_profile_id=NEW.execution_profile_id
          AND c.expires_unix>extract(epoch FROM clock_timestamp()) AND c.model_sha256=NEW.model_sha256 AND c.execution_profile_digest=NEW.execution_profile_digest
          AND NOT EXISTS(SELECT FROM model_registry_disables x WHERE x.model_version_id=a.model_version_id)) THEN
        RAISE EXCEPTION 'model has no active matching certificate' USING ERRCODE='23514',CONSTRAINT='model_version_namespace_conflict';
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER reserve_execution_model ON reviewed_execution_profiles;
CREATE TRIGGER reserve_execution_model BEFORE INSERT OR UPDATE OF enabled ON reviewed_execution_profiles FOR EACH ROW EXECUTE FUNCTION reserve_execution_model();
CREATE FUNCTION stop_disabled_model_profiles() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE reviewed_execution_profiles SET enabled=false WHERE model_version_id=NEW.model_version_id AND enabled;
    RETURN NEW;
END $$;
CREATE TRIGGER stop_disabled_model_profiles AFTER INSERT ON model_registry_disables FOR EACH ROW EXECUTE FUNCTION stop_disabled_model_profiles();

CREATE FUNCTION require_registry_price(profile text, price text) RETURNS timestamptz LANGUAGE plpgsql AS $$
DECLARE name text; cutoff timestamptz;
BEGIN
    SELECT model_version_id INTO name FROM reviewed_execution_profiles WHERE execution_profile_id=profile;
    IF NOT FOUND OR NOT EXISTS(SELECT FROM admin_model_drafts WHERE model_version_id=name) THEN RETURN NULL; END IF;
    PERFORM model_version_id FROM model_version_names WHERE model_version_id=name FOR SHARE;
    SELECT to_timestamp(c.expires_unix) INTO cutoff FROM registry_model_states d JOIN reviewed_execution_profiles p USING(model_version_id)
        JOIN model_certifications c ON c.report_id=d.certification_report_id AND c.execution_profile_id=p.execution_profile_id
        WHERE d.model_version_id=name AND d.status='active' AND p.enabled AND p.execution_profile_id=profile
        AND p.model_sha256=c.model_sha256 AND p.execution_profile_digest=c.execution_profile_digest
        AND d.manifest#>>'{pricing,policy}'='price_version' AND d.manifest#>>'{pricing,price_version}'=price;
    IF cutoff IS NULL OR cutoff<=clock_timestamp() THEN RAISE EXCEPTION 'model admission or price unavailable' USING ERRCODE='OA086'; END IF;
    RETURN cutoff;
END $$;
CREATE FUNCTION guard_registry_price() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.enabled THEN PERFORM require_registry_price(NEW.execution_profile_id,NEW.price_version); END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER check_model_text_price BEFORE INSERT OR UPDATE OF enabled ON text_prices FOR EACH ROW EXECUTE FUNCTION guard_registry_price();
CREATE TRIGGER check_model_audio_price BEFORE INSERT OR UPDATE OF enabled ON transcription_prices FOR EACH ROW EXECUTE FUNCTION guard_registry_price();
CREATE FUNCTION guard_registry_quote_plan() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE price text; cutoff timestamptz;
BEGIN
    SELECT price_version INTO price FROM quotes WHERE quote_id=NEW.quote_id;
    cutoff:=require_registry_price(NEW.execution_profile_id,price);
    IF cutoff IS NOT NULL THEN NEW.job_deadline:=least(NEW.job_deadline,cutoff); END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER check_registry_quote BEFORE INSERT ON quote_execution_plans FOR EACH ROW EXECUTE FUNCTION guard_registry_quote_plan();
CREATE FUNCTION guard_registry_job() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE binding record;
BEGIN
    SELECT qp.execution_profile_id,q.price_version INTO binding FROM quote_execution_plans qp JOIN quotes q USING(quote_id) WHERE qp.quote_id=NEW.quote_id;
    IF FOUND THEN PERFORM require_registry_price(binding.execution_profile_id,binding.price_version); END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER check_registry_job BEFORE INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION guard_registry_job();

-- Older gateways already honor job_deadline when issuing and renewing a lease.
-- New catalogue queries also stop advertising a managed profile as soon as its report expires.
CREATE FUNCTION registry_profile_current(profile text) RETURNS boolean LANGUAGE sql VOLATILE AS $$
    SELECT NOT EXISTS(SELECT FROM reviewed_execution_profiles p JOIN registry_model_states d USING(model_version_id)
        WHERE p.execution_profile_id=profile AND d.status<>'active')
$$;
