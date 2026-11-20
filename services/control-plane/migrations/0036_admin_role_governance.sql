-- Preserve a monotonically increasing revision even across deletion and
-- recreation of a scoped role. Existing bindings start at revision one.
CREATE TABLE admin_role_revisions (
  principal_id text NOT NULL REFERENCES admin_principals(principal_id),
  role text NOT NULL, environment text NOT NULL, scope_kind text NOT NULL, scope_id text NOT NULL,
  revision bigint NOT NULL CHECK(revision>0),
  PRIMARY KEY(principal_id,role,environment,scope_kind,scope_id)
);
INSERT INTO admin_role_revisions
  SELECT principal_id,role,environment,scope_kind,scope_id,1 FROM admin_grants;
CREATE FUNCTION record_admin_role_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='UPDATE' AND ROW(OLD.principal_id,OLD.role,OLD.environment,OLD.scope_kind,OLD.scope_id)
    IS DISTINCT FROM ROW(NEW.principal_id,NEW.role,NEW.environment,NEW.scope_kind,NEW.scope_id)
    THEN RAISE EXCEPTION 'role identity is immutable; use a separate grant'; END IF;
  IF TG_OP='UPDATE' AND NEW IS NOT DISTINCT FROM OLD THEN RETURN NULL; END IF;
  IF TG_OP='DELETE' THEN
    UPDATE admin_role_revisions SET revision=revision+1 WHERE principal_id=OLD.principal_id
      AND role=OLD.role AND environment=OLD.environment AND scope_kind=OLD.scope_kind AND scope_id=OLD.scope_id;
  ELSE
    INSERT INTO admin_role_revisions VALUES(NEW.principal_id,NEW.role,NEW.environment,NEW.scope_kind,NEW.scope_id,1)
      ON CONFLICT(principal_id,role,environment,scope_kind,scope_id)
      DO UPDATE SET revision=admin_role_revisions.revision+1;
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER admin_role_revision AFTER INSERT OR UPDATE OR DELETE ON admin_grants
  FOR EACH ROW EXECUTE FUNCTION record_admin_role_revision();
CREATE TABLE admin_role_witnesses (
  action_id text NOT NULL REFERENCES admin_action_control(action_id),
  phase text NOT NULL CHECK(phase IN('maker','checker')),
  principal_id text NOT NULL REFERENCES admin_principals(principal_id),
  authority_digest text NOT NULL CHECK(authority_digest ~ '^[0-9a-f]{64}$'),
  PRIMARY KEY(action_id,phase)
);
CREATE TRIGGER admin_role_witness_immutable BEFORE UPDATE OR DELETE ON admin_role_witnesses
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE FUNCTION advance_admin_activation_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.policy_version := GREATEST(NEW.policy_version,OLD.policy_version+1);
  RETURN NEW;
END $$;
CREATE TRIGGER admin_activation_revision BEFORE UPDATE OF active ON admin_principals
  FOR EACH ROW WHEN (OLD.active IS DISTINCT FROM NEW.active) EXECUTE FUNCTION advance_admin_activation_version();
