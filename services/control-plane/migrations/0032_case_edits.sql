ALTER TABLE reconciliation_cases
  ADD COLUMN assigned_to text REFERENCES admin_principals(principal_id),
  ADD COLUMN resolution_ref text;
CREATE TABLE reconciliation_case_edits (
  action_id text PRIMARY KEY REFERENCES admin_action_control(action_id),
  case_id text NOT NULL REFERENCES reconciliation_cases(case_id),
  case_version bigint NOT NULL CHECK(case_version > 1),
  actor_id text NOT NULL REFERENCES admin_principals(principal_id),
  occurred_at bigint NOT NULL,
  UNIQUE(case_id,case_version)
);
CREATE TRIGGER reconciliation_edit_immutable BEFORE UPDATE OR DELETE ON reconciliation_case_edits
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
