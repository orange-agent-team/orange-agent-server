ALTER TABLE jobs ADD COLUMN submitting_user_id TEXT REFERENCES users(user_id);
ALTER TABLE jobs ADD COLUMN input_required BOOLEAN NOT NULL DEFAULT FALSE;
CREATE TABLE job_inputs(
 job_id TEXT PRIMARY KEY REFERENCES jobs(job_id),
 artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),version_id TEXT NOT NULL,
 owner_id TEXT NOT NULL REFERENCES owners(owner_id),authorizing_user_id TEXT NOT NULL REFERENCES users(user_id),
 artifact_sha256 TEXT NOT NULL CHECK(artifact_sha256 ~ '^[0-9a-f]{64}$'),
 size_bytes BIGINT NOT NULL CHECK(size_bytes BETWEEN 1 AND 262144),
 projection_digest TEXT NOT NULL CHECK(projection_digest ~ '^[0-9a-f]{64}$'),
 organization_policy BIGINT,acl_version BIGINT,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK((organization_policy IS NULL)=(acl_version IS NULL))
);
CREATE TRIGGER job_input_immutable BEFORE UPDATE OR DELETE ON job_inputs FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION protect_job_input_author() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.submitting_user_id IS DISTINCT FROM OLD.submitting_user_id OR NEW.input_required!=OLD.input_required
 THEN RAISE EXCEPTION 'job submission authority is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER job_input_author_immutable BEFORE UPDATE ON jobs FOR EACH ROW EXECUTE FUNCTION protect_job_input_author();
ALTER TABLE worker_sessions ADD COLUMN input_artifacts_version INTEGER NOT NULL DEFAULT 0 CHECK(input_artifacts_version BETWEEN 0 AND 1);
-- Organization spend authority is explicit; content membership is not a grant
-- to consume every budget. Provisioning UI/approval remains a separate module.
CREATE TABLE budget_user_grants(
 root_budget_id TEXT NOT NULL REFERENCES budget_roots(root_budget_id),
 user_id TEXT NOT NULL REFERENCES users(user_id),can_submit BOOLEAN NOT NULL DEFAULT TRUE,
 PRIMARY KEY(root_budget_id,user_id)
);
