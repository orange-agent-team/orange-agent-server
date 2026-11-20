-- Selecting a model-visible definition conveys no source access. Only a later
-- per-operation approval may bind a live, separately issued Run grant.
CREATE TABLE cloud_connector_plans (
    run_id text PRIMARY KEY REFERENCES cloud_model_plans(run_id),
    tool_name text NOT NULL, connector_kind text NOT NULL,
    execution_digest text NOT NULL CHECK(execution_digest ~ '^[a-f0-9]{64}$'),
    CHECK ((connector_kind='postgres' AND tool_name IN ('list_views','describe_view','query_view'))
        OR (connector_kind='s3' AND tool_name IN ('list_objects','head_object','read_object')))
);
CREATE TRIGGER cloud_connector_plan_immutable BEFORE UPDATE OR DELETE ON cloud_connector_plans
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_connector_model_commits (
    run_id text PRIMARY KEY REFERENCES cloud_connector_plans(run_id),
    job_id text NOT NULL UNIQUE, commit_id text NOT NULL UNIQUE,
    artifact_id text NOT NULL, version_id text NOT NULL,
    response_digest text NOT NULL,
    FOREIGN KEY(job_id,run_id) REFERENCES cloud_model_jobs(job_id,run_id),
    FOREIGN KEY(commit_id,job_id) REFERENCES job_response_commits(commit_id,job_id),
    FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id)
);
CREATE TRIGGER cloud_connector_commit_immutable BEFORE UPDATE OR DELETE ON cloud_connector_model_commits
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_connector_decisions (
    run_id text PRIMARY KEY REFERENCES cloud_connector_model_commits(run_id),
    operation_id text NOT NULL UNIQUE, call_id text NOT NULL,
    argument_digest text NOT NULL, decision text NOT NULL CHECK(decision IN ('approve','reject')),
    grant_id text, request_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    CHECK ((grant_id IS NOT NULL)=(decision='approve'))
);
CREATE TRIGGER cloud_connector_decision_immutable BEFORE UPDATE OR DELETE ON cloud_connector_decisions
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_connector_effect_receipts (
    run_id text PRIMARY KEY REFERENCES cloud_connector_decisions(run_id),
    state text NOT NULL CHECK(state IN ('succeeded','failed','unknown','rejected')),
    result jsonb, result_digest text, finished_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((result IS NOT NULL)=(state='succeeded')),
    CHECK ((result_digest IS NOT NULL)=(state='succeeded'))
);
CREATE TRIGGER cloud_connector_effect_immutable BEFORE UPDATE OR DELETE ON cloud_connector_effect_receipts
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
