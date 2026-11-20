-- Optional first model turn. Plain-text workspace-only Runs retain their
-- existing contract and never acquire financial capabilities implicitly.
CREATE TABLE cloud_model_plans (
  run_id text PRIMARY KEY REFERENCES cloud_runs(run_id),
  owner_id text NOT NULL, invocation_id text NOT NULL,
  price_version text NOT NULL REFERENCES text_prices(price_version), model_version text NOT NULL,
  output_limit integer NOT NULL CHECK(output_limit BETWEEN 1 AND 4096),
  input_version_id text NOT NULL UNIQUE REFERENCES artifacts(version_id),
  UNIQUE(owner_id,invocation_id), FOREIGN KEY(run_id,owner_id) REFERENCES cloud_runs(run_id,owner_id)
);
CREATE TRIGGER cloud_model_plan_immutable BEFORE UPDATE OR DELETE ON cloud_model_plans
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_model_proposals (
  run_id text PRIMARY KEY REFERENCES cloud_model_plans(run_id),
  allocation_id text NOT NULL UNIQUE REFERENCES cloud_workspace_ready_receipts(allocation_id),
  input_receipt_id text NOT NULL UNIQUE REFERENCES cloud_workspace_input_reads(receipt_id),
  request_projection_digest text NOT NULL CHECK(request_projection_digest ~ '^[0-9a-f]{64}$'),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_model_proposal_immutable BEFORE UPDATE OR DELETE ON cloud_model_proposals
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE cloud_workspace_input_reads ADD COLUMN model_projection_digest text CHECK(model_projection_digest ~ '^[0-9a-f]{64}$');
CREATE TABLE cloud_model_quotes (
  quote_id text PRIMARY KEY REFERENCES quote_metering(quote_id),run_id text NOT NULL REFERENCES cloud_model_proposals(run_id),
  allocation_id text NOT NULL REFERENCES cloud_workspace_ready_receipts(allocation_id),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_model_quote_immutable BEFORE UPDATE OR DELETE ON cloud_model_quotes
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_model_jobs (
  job_id text PRIMARY KEY REFERENCES jobs(job_id),run_id text NOT NULL UNIQUE REFERENCES cloud_model_proposals(run_id),
  quote_id text NOT NULL UNIQUE REFERENCES cloud_model_quotes(quote_id)
);
CREATE TRIGGER cloud_model_job_immutable BEFORE UPDATE OR DELETE ON cloud_model_jobs
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_model_results (
  run_id text PRIMARY KEY REFERENCES cloud_model_proposals(run_id),
  job_id text NOT NULL UNIQUE REFERENCES cloud_model_jobs(job_id),
  commit_id text NOT NULL UNIQUE REFERENCES job_response_commits(commit_id),
  artifact_id text NOT NULL,version_id text NOT NULL,response_digest text NOT NULL,
  FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id)
);
CREATE TRIGGER cloud_model_result_immutable BEFORE UPDATE OR DELETE ON cloud_model_results
  FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

-- Cross-reference each downstream receipt to the same cloud turn and source,
-- even if a future caller supplies individually valid foreign identifiers.
ALTER TABLE cloud_workspace_ready_receipts ADD UNIQUE(allocation_id,input_receipt_id);
ALTER TABLE cloud_model_proposals ADD FOREIGN KEY(allocation_id,input_receipt_id)
 REFERENCES cloud_workspace_ready_receipts(allocation_id,input_receipt_id);
ALTER TABLE cloud_model_quotes ADD UNIQUE(quote_id,run_id);
ALTER TABLE cloud_model_jobs ADD FOREIGN KEY(quote_id,run_id) REFERENCES cloud_model_quotes(quote_id,run_id);
ALTER TABLE cloud_model_jobs ADD UNIQUE(job_id,run_id);
ALTER TABLE cloud_model_results ADD FOREIGN KEY(job_id,run_id) REFERENCES cloud_model_jobs(job_id,run_id);
ALTER TABLE job_response_commits ADD UNIQUE(commit_id,job_id);
ALTER TABLE cloud_model_results ADD FOREIGN KEY(commit_id,job_id) REFERENCES job_response_commits(commit_id,job_id);

ALTER TABLE cloud_model_jobs ADD FOREIGN KEY(job_id) REFERENCES job_inputs(job_id);
CREATE FUNCTION require_cloud_input_pin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM job_inputs i JOIN artifact_pins p ON p.artifact_id=i.artifact_id
    WHERE i.job_id=NEW.job_id AND p.reference_id='job-input:'||NEW.job_id) THEN RAISE EXCEPTION 'cloud Job input pin missing'; END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER cloud_job_input_pin AFTER INSERT ON cloud_model_jobs DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION require_cloud_input_pin();
