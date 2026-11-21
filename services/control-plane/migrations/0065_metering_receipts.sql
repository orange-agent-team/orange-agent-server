-- Trusted delivery identity is allocated by the response commit. Preserve the original key on every retry.
CREATE TABLE metering_retry_policies(
    policy_id text PRIMARY KEY, retry_seconds integer NOT NULL CHECK(retry_seconds>0),
    max_attempts integer NOT NULL CHECK(max_attempts BETWEEN 1 AND 32),
    base_delay_seconds integer NOT NULL CHECK(base_delay_seconds>0),
    max_delay_seconds integer NOT NULL CHECK(max_delay_seconds>=base_delay_seconds)
);
INSERT INTO metering_retry_policies VALUES ('development-v1',21600,12,5,900);
CREATE TRIGGER immutable_metering_policy BEFORE UPDATE OR DELETE ON metering_retry_policies
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();

CREATE TABLE metering_receipts(
    event_id text PRIMARY KEY REFERENCES job_response_commits(commit_id),
    job_id text UNIQUE NOT NULL REFERENCES metered_delivery_ids,owner_id text NOT NULL REFERENCES owners,
    response_digest text NOT NULL CHECK(response_digest ~ '^[0-9a-f]{64}$'),
    policy_id text NOT NULL REFERENCES metering_retry_policies, received_at timestamptz NOT NULL,
    deadline timestamptz NOT NULL, next_attempt timestamptz NOT NULL DEFAULT clock_timestamp(),
    state text NOT NULL CHECK(state IN ('received','validated','posted','dead')),
    attempts integer NOT NULL DEFAULT 0 CHECK(attempts>=0),
    generation bigint NOT NULL DEFAULT 1 CHECK(generation>0),
    case_id text REFERENCES reconciliation_cases,transaction_id text UNIQUE REFERENCES ledger_transactions,
    CHECK((state='posted')=(transaction_id IS NOT NULL)), CHECK(state<>'dead' OR case_id IS NOT NULL)
);
CREATE INDEX pending_metering_receipts ON metering_receipts(next_attempt,job_id) WHERE state IN('received','validated');
CREATE TABLE metering_receipt_history(
    sequence_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,event_id text NOT NULL REFERENCES metering_receipts,
    generation bigint NOT NULL,attempt integer NOT NULL,stage text NOT NULL CHECK(stage IN('received','validated','retry','dead','reopened','posted')),
    reason text NOT NULL, recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(event_id,generation,attempt,stage)
);
CREATE TRIGGER immutable_metering_history BEFORE UPDATE OR DELETE ON metering_receipt_history FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION register_metered_receipt(p_job text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
 SELECT c.commit_id,c.response_digest,c.committed_at,j.owner_id INTO STRICT r FROM job_response_commits c JOIN jobs j USING(job_id) WHERE c.job_id=p_job;
 INSERT INTO metering_receipts(event_id,job_id,owner_id,response_digest,policy_id,received_at,deadline,state)
 SELECT r.commit_id,p_job,r.owner_id,r.response_digest,policy_id,r.committed_at,r.committed_at+make_interval(secs=>retry_seconds),'received'
 FROM metering_retry_policies WHERE policy_id='development-v1';
 INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason,recorded_at) VALUES(r.commit_id,1,0,'received','committed_delivery',r.committed_at);
END $$;
CREATE FUNCTION receive_metering_fact() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM register_metered_receipt(NEW.job_id);RETURN NEW;END $$;
CREATE TRIGGER register_metering_event AFTER INSERT ON metered_delivery_ids FOR EACH ROW EXECUTE FUNCTION receive_metering_fact();
CREATE FUNCTION record_metering_post(p_job text,p_tx text,p_time timestamptz) RETURNS void LANGUAGE plpgsql AS $$
DECLARE receipt metering_receipts%ROWTYPE;
BEGIN
 SELECT * INTO STRICT receipt FROM metering_receipts WHERE job_id=p_job FOR UPDATE;
 IF receipt.state='dead' THEN RAISE EXCEPTION 'metering receipt requires authorized review';END IF;
 IF receipt.state='received' THEN
    -- Existing settlements and rolling-upgrade writers reached the original trusted settlement checks.
    INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason,recorded_at)
    VALUES(receipt.event_id,receipt.generation,receipt.attempts,'validated','accepted_settlement',p_time);
 END IF;
 UPDATE metering_receipts SET state='posted',transaction_id=p_tx WHERE job_id=p_job;
 INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason,recorded_at)
 VALUES(receipt.event_id,receipt.generation,receipt.attempts,'posted','ledger_transaction',p_time);
END $$;
CREATE FUNCTION finish_metering_fact() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM metering_receipts WHERE job_id=NEW.job_id AND (deadline<=clock_timestamp() OR next_attempt>clock_timestamp() OR state='dead')) THEN
    RAISE EXCEPTION 'metering retry window unavailable' USING ERRCODE='OA065';
 END IF;
 PERFORM record_metering_post(NEW.job_id,NEW.transaction_id,NEW.settled_at);RETURN NEW;
END $$;
CREATE TRIGGER record_metering_transaction AFTER INSERT ON job_settlements FOR EACH ROW EXECUTE FUNCTION finish_metering_fact();
DO $$
DECLARE delivery record;
BEGIN
 FOR delivery IN SELECT d.job_id,s.transaction_id,s.settled_at FROM metered_delivery_ids d LEFT JOIN job_settlements s USING(job_id) LOOP
   PERFORM register_metered_receipt(delivery.job_id);
   IF delivery.transaction_id IS NOT NULL THEN PERFORM record_metering_post(delivery.job_id,delivery.transaction_id,delivery.settled_at); END IF;
 END LOOP;
END $$;
CREATE FUNCTION protect_metering_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (OLD.event_id,OLD.job_id,OLD.owner_id,OLD.response_digest,OLD.policy_id,OLD.received_at)
    IS DISTINCT FROM (NEW.event_id,NEW.job_id,NEW.owner_id,NEW.response_digest,NEW.policy_id,NEW.received_at)
    OR OLD.state='posted' THEN RAISE EXCEPTION 'immutable metering identity or posted result'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER protect_metering_identity BEFORE UPDATE OR DELETE ON metering_receipts FOR EACH ROW EXECUTE FUNCTION protect_metering_receipt();

-- Invalid candidate bytes are never retained here. A bound upload is the stable identity before any usage fact exists.
CREATE TABLE metering_input_rejections(
    upload_id text PRIMARY KEY REFERENCES worker_output_uploads(upload_id), job_id text NOT NULL REFERENCES jobs,
    owner_id text NOT NULL REFERENCES owners, response_digest text NOT NULL CHECK(response_digest ~ '^[0-9a-f]{64}$'),
    bytes_digest text NOT NULL CHECK(bytes_digest ~ '^[0-9a-f]{64}$'),
    reason text NOT NULL CHECK(reason IN ('invalid_candidate','untrusted_owner','evidence_integrity')), case_id text UNIQUE NOT NULL REFERENCES reconciliation_cases,
    rejected_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_metering_input_rejection BEFORE UPDATE OR DELETE ON metering_input_rejections FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
