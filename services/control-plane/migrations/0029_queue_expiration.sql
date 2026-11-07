-- Financial completion for an expired Job that never received execution authority.
CREATE TABLE job_queue_expirations(
 job_id TEXT PRIMARY KEY REFERENCES jobs(job_id),
 quote_id TEXT NOT NULL REFERENCES quote_execution_plans(quote_id),
 job_deadline TIMESTAMPTZ NOT NULL,
 transaction_id TEXT NOT NULL UNIQUE REFERENCES ledger_transactions(transaction_id),
 occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER queue_expiration_immutable BEFORE UPDATE OR DELETE ON job_queue_expirations
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
