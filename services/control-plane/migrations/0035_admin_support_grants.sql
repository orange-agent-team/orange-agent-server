CREATE TABLE admin_support_grants (
  grant_id text PRIMARY KEY,
  sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  authorization_id text NOT NULL UNIQUE REFERENCES personal_support_consents(authorization_id),
  issue_action_id text NOT NULL UNIQUE REFERENCES admin_action_control(action_id),
  beneficiary_id text NOT NULL REFERENCES admin_principals(principal_id),
  checker_id text NOT NULL REFERENCES admin_principals(principal_id),
  expires_at bigint NOT NULL,
  CHECK (beneficiary_id<>checker_id)
);
CREATE TRIGGER admin_support_grants_immutable BEFORE UPDATE OR DELETE ON admin_support_grants
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE admin_support_revocations (
  grant_id text PRIMARY KEY REFERENCES admin_support_grants(grant_id),
  revoke_action_id text NOT NULL UNIQUE REFERENCES admin_action_control(action_id)
);
CREATE TRIGGER admin_support_revocations_immutable BEFORE UPDATE OR DELETE ON admin_support_revocations
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- Deduplication retains only a request digest and audit reference. Returning
-- content always requires a fresh live authorization, including on retry.
CREATE TABLE admin_support_reads (
  principal_id text NOT NULL REFERENCES admin_principals(principal_id),
  request_key text NOT NULL,
  request_digest text NOT NULL,
  audit_event_id text NOT NULL UNIQUE REFERENCES admin_audit(event_id),
  grant_id text NOT NULL REFERENCES admin_support_grants(grant_id),
  artifact_id text NOT NULL,
  version_id text NOT NULL,
  offset_bytes bigint NOT NULL CHECK(offset_bytes>=0),
  limit_bytes integer NOT NULL CHECK(limit_bytes BETWEEN 1 AND 65536),
  returned_bytes integer NOT NULL CHECK(returned_bytes BETWEEN 0 AND limit_bytes),
  FOREIGN KEY (artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id),
  PRIMARY KEY(principal_id,request_key)
);
CREATE TRIGGER admin_support_reads_immutable BEFORE UPDATE OR DELETE ON admin_support_reads
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- A later role grant or account reactivation must not resurrect an earlier
-- content capability. Keep historic approvals independently auditable.
CREATE TABLE admin_support_authority_losses (
  grant_id text PRIMARY KEY REFERENCES admin_support_grants(grant_id),
  lost_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER admin_support_loss_immutable BEFORE UPDATE OR DELETE ON admin_support_authority_losses
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX admin_support_beneficiary ON admin_support_grants(beneficiary_id);
CREATE INDEX admin_support_checker ON admin_support_grants(checker_id);
CREATE TABLE admin_support_invalidated_actions (
  action_id text PRIMARY KEY REFERENCES admin_action_control(action_id),
  lost_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER admin_support_invalidated_action_immutable BEFORE UPDATE OR DELETE ON admin_support_invalidated_actions
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX admin_support_pending_actions ON admin_actions(scope_id)
  WHERE action_type='support_access.grant' AND document->>'state' IN ('pending_approval','ready');
CREATE FUNCTION invalidate_scoped_support_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.environment='dev' AND OLD.scope_kind='tenant' AND OLD.role IN ('support_operator','security_operator') THEN
    IF TG_OP='DELETE' OR NEW.revoked OR OLD.expires_at<=EXTRACT(EPOCH FROM clock_timestamp())
       OR NEW.expires_at<=EXTRACT(EPOCH FROM clock_timestamp())
       OR ROW(NEW.principal_id,NEW.environment,NEW.scope_kind,NEW.scope_id,NEW.role)
          IS DISTINCT FROM ROW(OLD.principal_id,OLD.environment,OLD.scope_kind,OLD.scope_id,OLD.role) THEN
      INSERT INTO admin_support_invalidated_actions(action_id)
        SELECT a.action_id FROM admin_actions a WHERE a.action_type='support_access.grant' AND a.environment='dev'
          AND a.scope_kind='tenant' AND a.scope_id=OLD.scope_id AND a.document->>'state' IN ('pending_approval','ready')
          AND ((OLD.role='support_operator' AND a.document->>'requested_by'=OLD.principal_id)
            OR (OLD.role='security_operator' AND a.document->'approvals'->0->>'approver_id'=OLD.principal_id))
        ON CONFLICT DO NOTHING;
      INSERT INTO admin_support_authority_losses(grant_id)
        SELECT g.grant_id FROM admin_support_grants g JOIN personal_support_consents c USING(authorization_id)
        WHERE c.owner_id=OLD.scope_id AND (
          (OLD.role='support_operator' AND g.beneficiary_id=OLD.principal_id)
          OR (OLD.role='security_operator' AND g.checker_id=OLD.principal_id))
        ON CONFLICT DO NOTHING;
    END IF;
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER admin_support_role_loss AFTER UPDATE OR DELETE ON admin_grants
  FOR EACH ROW EXECUTE FUNCTION invalidate_scoped_support_grants();
CREATE FUNCTION invalidate_principal_support_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO admin_support_invalidated_actions(action_id)
    SELECT action_id FROM admin_actions WHERE action_type='support_access.grant'
      AND document->>'state' IN ('pending_approval','ready') AND
        (document->>'requested_by'=NEW.principal_id OR document->'approvals'->0->>'approver_id'=NEW.principal_id)
    ON CONFLICT DO NOTHING;
  INSERT INTO admin_support_authority_losses(grant_id)
    SELECT grant_id FROM admin_support_grants WHERE beneficiary_id=NEW.principal_id OR checker_id=NEW.principal_id
    ON CONFLICT DO NOTHING;
  RETURN NULL;
END $$;
CREATE TRIGGER admin_support_principal_loss AFTER UPDATE OF active ON admin_principals
  FOR EACH ROW WHEN (OLD.active AND NOT NEW.active) EXECUTE FUNCTION invalidate_principal_support_grants();
