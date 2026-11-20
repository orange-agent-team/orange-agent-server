-- Only new positive spending is restricted. Historical settlements, refunds, forensics and content grants retain their own rules.
-- Keep the wallets row type compatible with live v58 prepared queries.
CREATE TABLE test_spend_policy_fences(
 owner_id text PRIMARY KEY REFERENCES wallets(owner_id),generation bigint NOT NULL DEFAULT 0 CHECK(generation>=0)
);
LOCK TABLE wallets IN SHARE ROW EXCLUSIVE MODE;
INSERT INTO test_spend_policy_fences(owner_id) SELECT owner_id FROM wallets;
CREATE FUNCTION initialize_spend_fence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO test_spend_policy_fences(owner_id) VALUES(NEW.owner_id); RETURN NEW; END $$;
CREATE TRIGGER initialize_spend_fence AFTER INSERT ON wallets FOR EACH ROW EXECUTE FUNCTION initialize_spend_fence();
CREATE TABLE test_dispute_work(
 merchant_id text NOT NULL,dispute_id text NOT NULL,charge_id text NOT NULL,
 order_id text REFERENCES test_credit_fulfillments(order_id),owner_id text REFERENCES owners(owner_id),case_id text UNIQUE REFERENCES reconciliation_cases(case_id),
 state text NOT NULL DEFAULT 'unverified',blocks_spend boolean NOT NULL DEFAULT false,
 notified bigint NOT NULL DEFAULT 0,verified bigint NOT NULL DEFAULT 0,generation bigint NOT NULL DEFAULT 0,
 next_check timestamptz NOT NULL DEFAULT clock_timestamp(),claim_until timestamptz,
 PRIMARY KEY(merchant_id,dispute_id),CHECK(verified BETWEEN 0 AND notified),CHECK((owner_id IS NULL)=(order_id IS NULL)),CHECK(NOT blocks_spend OR owner_id IS NOT NULL)
);
CREATE INDEX test_dispute_owner_block ON test_dispute_work(owner_id) WHERE blocks_spend;
CREATE INDEX test_dispute_charge ON test_dispute_work(merchant_id,charge_id);
CREATE TABLE test_dispute_inbox(
 merchant_id text NOT NULL,event_id text NOT NULL,dispute_id text NOT NULL,charge_id text NOT NULL,kind text NOT NULL,sha256 text NOT NULL,
 received_at timestamptz NOT NULL DEFAULT clock_timestamp(),PRIMARY KEY(merchant_id,event_id),
 FOREIGN KEY(merchant_id,dispute_id) REFERENCES test_dispute_work(merchant_id,dispute_id)
);
CREATE TRIGGER immutable_test_dispute_event BEFORE UPDATE OR DELETE ON test_dispute_inbox FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_dispute_observations(
 merchant_id text NOT NULL,dispute_id text NOT NULL,generation bigint NOT NULL,notification_version bigint NOT NULL,
 state text NOT NULL,amount_minor bigint NOT NULL CHECK(amount_minor>0),currency text NOT NULL,charge_id text NOT NULL,payment_intent text NOT NULL,
 observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),PRIMARY KEY(merchant_id,dispute_id,generation),
 FOREIGN KEY(merchant_id,dispute_id) REFERENCES test_dispute_work(merchant_id,dispute_id)
);
CREATE TRIGGER immutable_test_dispute_observation BEFORE UPDATE OR DELETE ON test_dispute_observations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION deny_new_disputed_spend() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 PERFORM generation FROM test_spend_policy_fences WHERE owner_id=NEW.owner_id FOR SHARE;
 IF NOT FOUND THEN RAISE EXCEPTION 'missing purchase spend fence'; END IF;
 IF EXISTS(SELECT 1 FROM test_dispute_work WHERE owner_id=NEW.owner_id AND blocks_spend) THEN
   RAISE EXCEPTION USING ERRCODE='OA031',MESSAGE='new paid jobs paused for payment review';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER disputed_owner_new_reservation BEFORE INSERT ON jobs FOR EACH ROW WHEN(NEW.reserved>0) EXECUTE FUNCTION deny_new_disputed_spend();
CREATE FUNCTION deny_disputed_purchase_credit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE merchant text;
BEGIN
 SELECT merchant_id INTO STRICT merchant FROM test_checkout_orders WHERE order_id=NEW.order_id;
 PERFORM pg_advisory_xact_lock(hashtextextended(merchant||':'||NEW.charge_id,61));
 IF EXISTS(SELECT 1 FROM test_dispute_work WHERE merchant_id=merchant AND charge_id=NEW.charge_id AND state NOT IN('won','warning_closed','prevented')) THEN
    RAISE EXCEPTION USING ERRCODE='OA032',MESSAGE='original payment dispute unresolved';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER disputed_payment_fulfillment BEFORE INSERT ON test_credit_fulfillments FOR EACH ROW EXECUTE FUNCTION deny_disputed_purchase_credit();
-- One merchant event cannot acquire a second meaning via a different business handler.
CREATE TABLE test_merchant_event_ids(
 merchant_id text NOT NULL,event_id text NOT NULL,kind text NOT NULL,external_object text NOT NULL,
 PRIMARY KEY(merchant_id,event_id)
);
INSERT INTO test_merchant_event_ids SELECT merchant_id,event_id,kind,session_id FROM test_stripe_inbox
UNION ALL SELECT merchant_id,event_id,kind,provider_refund_id FROM test_stripe_refund_events;
CREATE TRIGGER immutable_test_event_identity BEFORE UPDATE OR DELETE ON test_merchant_event_ids FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION register_test_event_identity() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE external_id text;
BEGIN
 external_id=to_jsonb(NEW)->>TG_ARGV[0];
 INSERT INTO test_merchant_event_ids VALUES(NEW.merchant_id,NEW.event_id,NEW.kind,external_id) ON CONFLICT DO NOTHING;
 IF NOT EXISTS(SELECT 1 FROM test_merchant_event_ids i WHERE i.merchant_id=NEW.merchant_id AND i.event_id=NEW.event_id AND i.kind=NEW.kind AND i.external_object=external_id)
 THEN RAISE EXCEPTION USING ERRCODE='OA034',MESSAGE='merchant event identity conflict'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER checkout_event_identity BEFORE INSERT ON test_stripe_inbox FOR EACH ROW EXECUTE FUNCTION register_test_event_identity('session_id');
CREATE TRIGGER refund_event_identity BEFORE INSERT ON test_stripe_refund_events FOR EACH ROW EXECUTE FUNCTION register_test_event_identity('provider_refund_id');
CREATE TRIGGER dispute_event_identity BEFORE INSERT ON test_dispute_inbox FOR EACH ROW EXECUTE FUNCTION register_test_event_identity('dispute_id');
