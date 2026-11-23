-- Existing snapshots start at revision 1. The database orders visible changes
-- for every writer, including cancellations, Worker receipts and settlement.
ALTER TABLE jobs ADD COLUMN state_revision BIGINT NOT NULL DEFAULT 1 CHECK (state_revision > 0);

CREATE FUNCTION advance_job_state_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_revision <> 1 THEN
            RAISE EXCEPTION 'a new job must start at revision 1' USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.state_revision <> OLD.state_revision THEN
        RAISE EXCEPTION 'job state revision is managed by the database' USING ERRCODE = '23514';
    END IF;
    IF OLD.execution_state IN ('succeeded','failed','cancelled') AND NEW.execution_state <> OLD.execution_state THEN
        RAISE EXCEPTION 'terminal job execution is immutable' USING ERRCODE = '23514';
    END IF;
    IF ROW(NEW.owner_id, NEW.root_budget_id, NEW.invocation_id, NEW.purpose, NEW.execution_state, NEW.settlement_state, NEW.reserved, NEW.charge)
       IS DISTINCT FROM
       ROW(OLD.owner_id, OLD.root_budget_id, OLD.invocation_id, OLD.purpose, OLD.execution_state, OLD.settlement_state, OLD.reserved, OLD.charge) THEN
        NEW.state_revision := OLD.state_revision + 1;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER job_state_revision BEFORE INSERT OR UPDATE ON jobs
FOR EACH ROW EXECUTE FUNCTION advance_job_state_revision();
