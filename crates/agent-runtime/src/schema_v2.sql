-- Rebuild only operations to extend its state CHECK. Foreign keys are disabled
-- outside this transaction and checked before COMMIT by the caller.
ALTER TABLE responses ADD COLUMN body TEXT;
CREATE TABLE operations_new(
 operation_id TEXT PRIMARY KEY,owner TEXT NOT NULL,
 run_id TEXT NOT NULL REFERENCES runs(run_id),
 invocation TEXT NOT NULL REFERENCES responses(invocation),
 argument_digest TEXT NOT NULL,tool_version TEXT NOT NULL,
 environment TEXT NOT NULL,policy TEXT NOT NULL,
 requires_approval INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN('intended','dispatching','succeeded','failed','unknown')),
 result_digest TEXT,request_json TEXT,call_id TEXT,ordinal INTEGER,
 result_json TEXT,model_result TEXT,error_code TEXT,
 context_applied INTEGER NOT NULL DEFAULT 0 CHECK(context_applied IN(0,1))
);
INSERT INTO operations_new(operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state,result_digest)
 SELECT operation_id,owner,run_id,invocation,argument_digest,tool_version,environment,policy,requires_approval,state,result_digest FROM operations;
DROP TABLE operations;
ALTER TABLE operations_new RENAME TO operations;
CREATE UNIQUE INDEX operation_call ON operations(invocation,call_id) WHERE call_id IS NOT NULL;
CREATE TABLE runtime_runs(
 run_id TEXT PRIMARY KEY REFERENCES runs(run_id),config TEXT NOT NULL,config_digest TEXT NOT NULL,
 environment TEXT NOT NULL,messages TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN('ready','model_pending','tools_pending','completed','cancelled','limited','failed','recovery_required')),
 invocations INTEGER NOT NULL DEFAULT 0,steps INTEGER NOT NULL DEFAULT 0,active_invocation TEXT,final_text TEXT,failure TEXT
);
CREATE TABLE runtime_invocations(
 invocation TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES runtime_runs(run_id),
 ordinal INTEGER NOT NULL,request TEXT NOT NULL,request_digest TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('intended','dispatched','committed','failed','unknown')),
 epoch INTEGER NOT NULL,fencing INTEGER NOT NULL,attempt INTEGER NOT NULL,
 UNIQUE(run_id,ordinal)
);
CREATE TABLE runtime_host(singleton INTEGER PRIMARY KEY CHECK(singleton=1),epoch INTEGER NOT NULL CHECK(epoch>0));
INSERT INTO runtime_host VALUES(1,1);
PRAGMA user_version=2;
