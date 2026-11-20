-- Keep workspace preparation, Run execution, model charging and tool dispatch
-- independently fenced. Already-created model plans retain their original mode.
ALTER TABLE cloud_model_plans ADD COLUMN execution_protocol int NOT NULL DEFAULT 0 CHECK(execution_protocol IN (0,1));
ALTER TABLE cloud_model_plans ALTER COLUMN execution_protocol SET DEFAULT 1;
ALTER TABLE cloud_pool_hosts ADD COLUMN runstore_version int NOT NULL DEFAULT 0 CHECK(runstore_version IN (0,1));
CREATE OR REPLACE FUNCTION protect_cloud_host_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (NOT OLD.enabled AND NEW.enabled) OR
    (to_jsonb(OLD)-ARRAY['enabled','live_until','accepting','input_protocol_version','runstore_version'])
    IS DISTINCT FROM (to_jsonb(NEW)-ARRAY['enabled','live_until','accepting','input_protocol_version','runstore_version']) THEN
   RAISE EXCEPTION 'retired platform host identity cannot be reused';
 END IF;
 RETURN NEW;
END $$;

CREATE TABLE cloud_run_leases (
 run_id text PRIMARY KEY REFERENCES cloud_model_proposals(run_id),
 allocation_id text NOT NULL UNIQUE REFERENCES cloud_workspace_ready_receipts(allocation_id),
 host_instance_id text NOT NULL REFERENCES cloud_pool_hosts(host_instance_id),
 epoch bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
 command_id text NOT NULL,request_version bigint NOT NULL CHECK(request_version>=1),
 issued_version bigint NOT NULL CHECK(issued_version>=1),lease_until bigint NOT NULL,
 previous_version bigint,previous_until bigint,
 UNIQUE(host_instance_id,command_id),CHECK((previous_version IS NULL)=(previous_until IS NULL))
);
CREATE TABLE cloud_directory_capabilities (
 run_id text PRIMARY KEY REFERENCES cloud_model_plans(run_id),capability_id text NOT NULL UNIQUE,
 workspace_id text NOT NULL UNIQUE REFERENCES cloud_workspaces(workspace_id),
 tool_version text NOT NULL,execution_digest text NOT NULL,policy_version text NOT NULL,
 created_by text NOT NULL REFERENCES users(user_id)
);
CREATE TRIGGER cloud_directory_capability_immutable BEFORE UPDATE OR DELETE ON cloud_directory_capabilities
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_tool_responses (
 run_id text PRIMARY KEY REFERENCES cloud_directory_capabilities(run_id),job_id text NOT NULL UNIQUE,
 commit_id text NOT NULL UNIQUE,artifact_id text NOT NULL,version_id text NOT NULL,response_digest text NOT NULL,
 FOREIGN KEY(job_id,run_id) REFERENCES cloud_model_jobs(job_id,run_id),
 FOREIGN KEY(commit_id,job_id) REFERENCES job_response_commits(commit_id,job_id),
 FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id)
);
CREATE TRIGGER cloud_tool_response_immutable BEFORE UPDATE OR DELETE ON cloud_tool_responses
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_operation_intents (
 operation_id text PRIMARY KEY,run_id text NOT NULL REFERENCES cloud_tool_responses(run_id),
 call_id text NOT NULL,commit_id text NOT NULL REFERENCES cloud_tool_responses(commit_id),
 capability_id text NOT NULL REFERENCES cloud_directory_capabilities(capability_id),
 approval_id text NOT NULL UNIQUE,tool_version text NOT NULL,policy_version text NOT NULL,environment_id text NOT NULL,
 args jsonb NOT NULL,argument_digest text NOT NULL,approval_digest text NOT NULL,
 UNIQUE(run_id,call_id),created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_operation_intent_immutable BEFORE UPDATE OR DELETE ON cloud_operation_intents
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_operation_state (
 operation_id text PRIMARY KEY REFERENCES cloud_operation_intents(operation_id),
 state text NOT NULL CHECK(state IN ('intended','dispatching','succeeded','failed','unknown')),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0)
);
CREATE TABLE cloud_operation_decisions (
 operation_id text PRIMARY KEY REFERENCES cloud_operation_intents(operation_id),user_id text NOT NULL REFERENCES users(user_id),
 decision text NOT NULL CHECK(decision IN ('approve','reject')),approval_digest text NOT NULL,expires_at bigint NOT NULL,request_key text NOT NULL,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),UNIQUE(user_id,request_key)
);
CREATE TRIGGER cloud_operation_decision_immutable BEFORE UPDATE OR DELETE ON cloud_operation_decisions
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_dispatch_permits (
 operation_id text PRIMARY KEY REFERENCES cloud_operation_decisions(operation_id),permit_id text NOT NULL UNIQUE,
 execution_epoch bigint NOT NULL REFERENCES cloud_run_leases(epoch),allocation_id text NOT NULL REFERENCES cloud_workspace_claims(allocation_id),
 expires_at bigint NOT NULL,created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_dispatch_permit_immutable BEFORE UPDATE OR DELETE ON cloud_dispatch_permits
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE cloud_operation_results (
 operation_id text PRIMARY KEY REFERENCES cloud_dispatch_permits(operation_id),
 state text NOT NULL CHECK(state IN ('succeeded','failed','unknown')),artifact_id text UNIQUE,
 result jsonb,result_digest text NOT NULL,created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK((state='succeeded')=(artifact_id IS NOT NULL AND result IS NOT NULL))
);
CREATE TRIGGER cloud_operation_result_immutable BEFORE UPDATE OR DELETE ON cloud_operation_results
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
ALTER TABLE cloud_operation_decisions ADD COLUMN reviewed_version bigint NOT NULL CHECK(reviewed_version>0);
CREATE TABLE cloud_directory_revocations (
 run_id text PRIMARY KEY REFERENCES cloud_directory_capabilities(run_id),receipt_id text NOT NULL UNIQUE,
 user_id text NOT NULL REFERENCES users(user_id),created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER cloud_directory_revocation_immutable BEFORE UPDATE OR DELETE ON cloud_directory_revocations
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION protect_cloud_operation_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.operation_id<>NEW.operation_id OR NEW.version<>OLD.version+1
    OR NOT ((OLD.state='intended' AND NEW.state IN ('intended','failed','dispatching'))
         OR (OLD.state='dispatching' AND NEW.state IN ('succeeded','failed','unknown'))) THEN
   RAISE EXCEPTION 'invalid tool operation transition';END IF;
 IF NEW.state='dispatching' AND NOT EXISTS(SELECT 1 FROM cloud_dispatch_permits WHERE operation_id=NEW.operation_id) THEN
   RAISE EXCEPTION 'durable dispatch permit required';END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER cloud_tool_transition BEFORE UPDATE OR DELETE ON cloud_operation_state FOR EACH ROW EXECUTE FUNCTION protect_cloud_operation_transition();
