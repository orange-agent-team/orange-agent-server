-- Initial Runs retain the original single-turn behavior. A new explicit
-- option keeps a prepared workspace for one user-funded continuation.
CREATE TABLE cloud_continuation_options (
 run_id text PRIMARY KEY REFERENCES cloud_directory_capabilities(run_id),
 authorized_by text NOT NULL REFERENCES users(user_id)
);
CREATE TRIGGER cloud_continuation_option_immutable BEFORE UPDATE OR DELETE ON cloud_continuation_options
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_continuation_intents (
 run_id text PRIMARY KEY REFERENCES cloud_continuation_options(run_id),turn_id text NOT NULL UNIQUE,
 operation_id text NOT NULL UNIQUE REFERENCES cloud_operation_results(operation_id),
 owner_id text NOT NULL REFERENCES owners(owner_id),user_id text NOT NULL REFERENCES users(user_id),
 original_version text NOT NULL REFERENCES artifacts(version_id),model_response_version text NOT NULL REFERENCES artifacts(version_id),
 operation_digest text NOT NULL,invocation_id text NOT NULL,upload_id text NOT NULL UNIQUE,
 request_key text NOT NULL,created_at timestamptz NOT NULL DEFAULT clock_timestamp(),UNIQUE(owner_id,invocation_id),UNIQUE(owner_id,request_key),
 FOREIGN KEY(run_id,owner_id) REFERENCES cloud_runs(run_id,owner_id)
);
CREATE TRIGGER cloud_continuation_intent_immutable BEFORE UPDATE OR DELETE ON cloud_continuation_intents
 FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_model_turns (
 turn_id text PRIMARY KEY,run_id text NOT NULL REFERENCES cloud_model_proposals(run_id),ordinal integer NOT NULL CHECK(ordinal IN (1,2)),
 owner_id text NOT NULL,invocation_id text NOT NULL,upload_id text NOT NULL,artifact_id text NOT NULL,version_id text NOT NULL UNIQUE,
 projection_digest text NOT NULL CHECK(projection_digest ~ '^[a-f0-9]{64}$'),parent_operation text REFERENCES cloud_operation_results(operation_id),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),UNIQUE(run_id,ordinal),UNIQUE(owner_id,invocation_id),UNIQUE(turn_id,run_id),
 CHECK((ordinal=1)=(parent_operation IS NULL)),FOREIGN KEY(run_id,owner_id) REFERENCES cloud_runs(run_id,owner_id),
 FOREIGN KEY(artifact_id,version_id) REFERENCES artifacts(artifact_id,version_id)
);
INSERT INTO cloud_model_turns(turn_id,run_id,ordinal,owner_id,invocation_id,upload_id,artifact_id,version_id,projection_digest)
 SELECT p.run_id,p.run_id,1,r.owner_id,m.invocation_id,u.upload_id,r.input_artifact_id,r.input_version_id,p.request_projection_digest
 FROM cloud_model_proposals p JOIN cloud_runs r USING(run_id) JOIN cloud_model_plans m USING(run_id,owner_id)
 JOIN artifact_uploads u ON u.owner_id=r.owner_id AND u.artifact_id=r.input_artifact_id;
CREATE FUNCTION record_initial_cloud_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO cloud_model_turns(turn_id,run_id,ordinal,owner_id,invocation_id,upload_id,artifact_id,version_id,projection_digest)
  SELECT r.run_id,r.run_id,1,r.owner_id,p.invocation_id,u.upload_id,r.input_artifact_id,r.input_version_id,NEW.request_projection_digest
  FROM cloud_runs r JOIN cloud_model_plans p USING(run_id,owner_id) JOIN artifact_uploads u ON u.owner_id=r.owner_id AND u.artifact_id=r.input_artifact_id
  WHERE r.run_id=NEW.run_id;
 IF NOT FOUND THEN RAISE EXCEPTION 'missing first-turn source'; END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER cloud_first_turn AFTER INSERT ON cloud_model_proposals FOR EACH ROW EXECUTE FUNCTION record_initial_cloud_turn();
CREATE TRIGGER cloud_turn_immutable BEFORE UPDATE OR DELETE ON cloud_model_turns FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_quote_turns (
 quote_id text PRIMARY KEY REFERENCES cloud_model_quotes(quote_id),turn_id text NOT NULL,run_id text NOT NULL,
 FOREIGN KEY(turn_id,run_id) REFERENCES cloud_model_turns(turn_id,run_id),
 FOREIGN KEY(quote_id,run_id) REFERENCES cloud_model_quotes(quote_id,run_id),UNIQUE(quote_id,turn_id,run_id)
);
INSERT INTO cloud_quote_turns SELECT q.quote_id,t.turn_id,q.run_id FROM cloud_model_quotes q JOIN cloud_model_turns t USING(run_id) WHERE t.ordinal=1;
CREATE FUNCTION record_cloud_quote_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO cloud_quote_turns(quote_id,turn_id,run_id)
 SELECT NEW.quote_id,t.turn_id,t.run_id FROM cloud_model_turns t JOIN quote_metering m ON m.input_version_id=t.version_id AND m.input_artifact_id=t.artifact_id
 JOIN quotes q ON q.quote_id=m.quote_id AND q.owner_id=t.owner_id AND q.invocation_id=t.invocation_id AND q.projection_digest=t.projection_digest
 WHERE t.run_id=NEW.run_id AND q.quote_id=NEW.quote_id;
 IF NOT FOUND THEN RAISE EXCEPTION 'quote does not refer to the declared turn'; END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER cloud_quote_turn AFTER INSERT ON cloud_model_quotes FOR EACH ROW EXECUTE FUNCTION record_cloud_quote_turn();
CREATE TRIGGER cloud_quote_turn_immutable BEFORE UPDATE OR DELETE ON cloud_quote_turns FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_job_turns (
 job_id text PRIMARY KEY,turn_id text NOT NULL UNIQUE,run_id text NOT NULL,quote_id text NOT NULL,
 FOREIGN KEY(job_id,run_id) REFERENCES cloud_model_jobs(job_id,run_id),
 FOREIGN KEY(quote_id,turn_id,run_id) REFERENCES cloud_quote_turns(quote_id,turn_id,run_id)
);
INSERT INTO cloud_job_turns SELECT j.job_id,q.turn_id,j.run_id,j.quote_id FROM cloud_model_jobs j JOIN cloud_quote_turns q USING(quote_id,run_id);
ALTER TABLE cloud_model_jobs DROP CONSTRAINT cloud_model_jobs_run_id_key;
CREATE FUNCTION record_cloud_job_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO cloud_job_turns(job_id,turn_id,run_id,quote_id)
 SELECT NEW.job_id,t.turn_id,NEW.run_id,NEW.quote_id FROM cloud_quote_turns t WHERE t.quote_id=NEW.quote_id AND t.run_id=NEW.run_id;
 IF NOT FOUND THEN RAISE EXCEPTION 'job has no bound model turn'; END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER cloud_job_turn AFTER INSERT ON cloud_model_jobs FOR EACH ROW EXECUTE FUNCTION record_cloud_job_turn();
CREATE TRIGGER cloud_job_turn_immutable BEFORE UPDATE OR DELETE ON cloud_job_turns FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE cloud_model_outcomes (
 turn_id text PRIMARY KEY REFERENCES cloud_job_turns(turn_id),job_id text NOT NULL UNIQUE REFERENCES cloud_model_jobs(job_id),
 kind text NOT NULL CHECK(kind IN ('final','tool_request','stopped')),created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO cloud_model_outcomes(turn_id,job_id,kind) SELECT t.turn_id,r.job_id,'final' FROM cloud_model_results r JOIN cloud_job_turns t USING(job_id);
INSERT INTO cloud_model_outcomes(turn_id,job_id,kind) SELECT t.turn_id,r.job_id,'tool_request' FROM cloud_tool_responses r JOIN cloud_job_turns t USING(job_id);
CREATE TRIGGER cloud_model_outcome_immutable BEFORE UPDATE OR DELETE ON cloud_model_outcomes FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION verify_cloud_followup_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.ordinal=2 AND NOT EXISTS(SELECT 1 FROM cloud_continuation_intents i WHERE i.run_id=NEW.run_id AND i.turn_id=NEW.turn_id
   AND i.upload_id=NEW.upload_id AND i.operation_id=NEW.parent_operation AND i.owner_id=NEW.owner_id AND i.invocation_id=NEW.invocation_id)
 THEN RAISE EXCEPTION 'missing immutable continuation intent';END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER cloud_followup_identity BEFORE INSERT ON cloud_model_turns FOR EACH ROW EXECUTE FUNCTION verify_cloud_followup_identity();
