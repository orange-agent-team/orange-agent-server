ALTER TABLE reconciliation_cases ADD COLUMN case_sequence bigint GENERATED ALWAYS AS IDENTITY;
CREATE UNIQUE INDEX reconciliation_case_sequence ON reconciliation_cases(case_sequence);
CREATE INDEX reconciliation_case_environment_page ON reconciliation_cases(environment,case_sequence DESC);
CREATE INDEX reconciliation_case_owner_page ON reconciliation_cases(environment,owner_id,case_sequence DESC);
CREATE TRIGGER reconciliation_case_sequence_immutable BEFORE UPDATE OF case_sequence
ON reconciliation_cases FOR EACH ROW WHEN (NEW.case_sequence IS DISTINCT FROM OLD.case_sequence)
EXECUTE FUNCTION forbid_ledger_mutation();
