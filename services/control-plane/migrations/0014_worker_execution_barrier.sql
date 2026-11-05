-- Cloud assignment/start barrier only. No settlement, automatic retry or cleanup.
ALTER TABLE worker_sessions ADD COLUMN execution_control_version INTEGER NOT NULL DEFAULT 0 CHECK(execution_control_version BETWEEN 0 AND 1);
CREATE TABLE reviewed_execution_profiles (
 execution_profile_id TEXT PRIMARY KEY,
 model_version_id TEXT NOT NULL,
 model_sha256 TEXT NOT NULL CHECK(model_sha256 ~ '^[0-9a-f]{64}$'),
 execution_profile_digest TEXT NOT NULL CHECK(execution_profile_digest ~ '^[0-9a-f]{64}$'),
 enabled BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE FUNCTION protect_execution_profile() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'execution profiles cannot be deleted'; END IF;
 IF ROW(OLD.execution_profile_id,OLD.model_version_id,OLD.model_sha256,OLD.execution_profile_digest)
 IS DISTINCT FROM ROW(NEW.execution_profile_id,NEW.model_version_id,NEW.model_sha256,NEW.execution_profile_digest)
 THEN RAISE EXCEPTION 'reviewed execution profile is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER execution_profile_identity BEFORE UPDATE OR DELETE ON reviewed_execution_profiles
FOR EACH ROW EXECUTE FUNCTION protect_execution_profile();
-- Trusted Quote/scheduler output, never accepted from Worker Hello or ACK. No
-- public provisioning endpoint exists in this slice. Existing quotes without a
-- bound plan remain queued. Input object ownership/delivery needs its own gate:
-- this initial barrier intentionally accepts only zero input artifacts.
CREATE TABLE quote_execution_plans (
 quote_id TEXT PRIMARY KEY REFERENCES quotes(quote_id),
 target_node_id TEXT NOT NULL REFERENCES nodes(node_id),
 execution_profile_id TEXT NOT NULL REFERENCES reviewed_execution_profiles(execution_profile_id),
 request_projection_digest TEXT NOT NULL CHECK(request_projection_digest ~ '^[0-9a-f]{64}$'),
 input_artifact_ids TEXT[] NOT NULL DEFAULT '{}' CHECK(cardinality(input_artifact_ids)=0),
 job_deadline TIMESTAMPTZ NOT NULL
);
CREATE TRIGGER quote_execution_plans_immutable BEFORE UPDATE OR DELETE ON quote_execution_plans
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE worker_execution_attempts (
 attempt_id TEXT PRIMARY KEY,
 job_id TEXT NOT NULL UNIQUE REFERENCES jobs(job_id),
 command_id TEXT NOT NULL UNIQUE,
 node_id TEXT NOT NULL REFERENCES nodes(node_id),
 session_id TEXT NOT NULL,
 connection_epoch BIGINT NOT NULL CHECK(connection_epoch>0),
 policy_generation BIGINT NOT NULL CHECK(policy_generation>=0),
 fencing_token BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
 lease_id TEXT NOT NULL UNIQUE,
 lease_expires_at TIMESTAMPTZ NOT NULL,
 ack_deadline TIMESTAMPTZ NOT NULL,
 last_sent_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 payload_digest TEXT NOT NULL CHECK(payload_digest ~ '^[0-9a-f]{64}$'),
 assignment BYTEA NOT NULL,
 state TEXT NOT NULL DEFAULT 'offered' CHECK(state IN ('offered','started','rejected','unknown')),
 ack_digest TEXT,
 execution_journal_id TEXT,
 broker_grant_id TEXT,
 broker_epoch BIGINT CHECK(broker_epoch>0),
 slot_generation BIGINT CHECK(slot_generation>0),
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 CHECK(ack_deadline<=lease_expires_at),
 CHECK((state='started') IS NOT TRUE OR (ack_digest IS NOT NULL AND execution_journal_id IS NOT NULL AND broker_grant_id IS NOT NULL AND broker_epoch IS NOT NULL AND slot_generation IS NOT NULL))
);
-- Unknown is occupied, never free. Rejected is safe only before StartGrant.
CREATE UNIQUE INDEX worker_one_unresolved_attempt ON worker_execution_attempts(node_id) WHERE state IN ('offered','started','unknown');
CREATE TABLE worker_execution_events (
 event_id TEXT PRIMARY KEY,
 attempt_id TEXT NOT NULL REFERENCES worker_execution_attempts(attempt_id),
 kind TEXT NOT NULL,
 occurred_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER worker_execution_events_immutable BEFORE UPDATE OR DELETE ON worker_execution_events
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
