-- Extend only the Run phase CHECK, preserving existing references and data.
CREATE TABLE runtime_runs_new(
 run_id TEXT PRIMARY KEY REFERENCES runs(run_id),config TEXT NOT NULL,config_digest TEXT NOT NULL,
 environment TEXT NOT NULL,messages TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN('ready','model_pending','tools_pending','awaiting_user','completed','cancelled','limited','failed','recovery_required')),
 invocations INTEGER NOT NULL DEFAULT 0,steps INTEGER NOT NULL DEFAULT 0,active_invocation TEXT,final_text TEXT,failure TEXT
);
INSERT INTO runtime_runs_new(rowid,run_id,config,config_digest,environment,messages,phase,invocations,steps,active_invocation,final_text,failure)
 SELECT rowid,run_id,config,config_digest,environment,messages,phase,invocations,steps,active_invocation,final_text,failure FROM runtime_runs;
DROP TABLE runtime_runs;
ALTER TABLE runtime_runs_new RENAME TO runtime_runs;
CREATE TABLE runtime_capabilities(
 run_id TEXT PRIMARY KEY REFERENCES runtime_runs(run_id),
 user_questions INTEGER NOT NULL CHECK(user_questions IN(0,1))
);
CREATE TRIGGER runtime_capability_immutable BEFORE UPDATE ON runtime_capabilities
 BEGIN SELECT RAISE(ABORT,'immutable run capabilities'); END;
CREATE TABLE runtime_questions(
 question_id TEXT PRIMARY KEY,run_id TEXT NOT NULL REFERENCES runtime_runs(run_id),
 invocation TEXT NOT NULL UNIQUE REFERENCES responses(invocation),call_id TEXT NOT NULL,
 body TEXT NOT NULL,argument_digest TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('pending','answered','cancelled')),
 answers TEXT,answer_digest TEXT,
 CHECK((state='answered' AND answers IS NOT NULL AND answer_digest IS NOT NULL)
       OR(state IN('pending','cancelled') AND answers IS NULL AND answer_digest IS NULL))
);
CREATE UNIQUE INDEX one_pending_question ON runtime_questions(run_id) WHERE state='pending';
CREATE TRIGGER question_identity_immutable BEFORE UPDATE OF question_id,run_id,invocation,call_id,body,argument_digest ON runtime_questions
 BEGIN SELECT RAISE(ABORT,'immutable question identity'); END;
CREATE TRIGGER question_terminal_immutable BEFORE UPDATE ON runtime_questions
 WHEN OLD.state!='pending' OR NEW.state NOT IN('answered','cancelled')
 BEGIN SELECT RAISE(ABORT,'question already decided'); END;
PRAGMA user_version=4;
