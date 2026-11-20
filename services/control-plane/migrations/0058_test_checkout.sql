-- Sandbox merchant catalogue and permanent platform order identity.
CREATE TABLE test_credit_offers (
 merchant_id text NOT NULL, offer_id text NOT NULL, stripe_price text NOT NULL,
 currency text NOT NULL CHECK(currency ~ '^[a-z]{3}$'), amount_minor bigint NOT NULL CHECK(amount_minor BETWEEN 1 AND 99999999),
 microcredits bigint NOT NULL CHECK(microcredits>0), enabled boolean NOT NULL DEFAULT false,
 PRIMARY KEY(merchant_id,offer_id), UNIQUE(merchant_id,stripe_price)
);
CREATE TRIGGER immutable_test_offer BEFORE UPDATE OR DELETE ON test_credit_offers FOR EACH ROW EXECUTE FUNCTION protect_transcription_price();
CREATE TABLE test_checkout_orders (
 order_id text PRIMARY KEY, owner_id text NOT NULL REFERENCES owners(owner_id), requester_id text NOT NULL REFERENCES users(user_id),
 merchant_id text NOT NULL, offer_id text NOT NULL, terms_digest text NOT NULL CHECK(terms_digest ~ '^[a-f0-9]{64}$'),
 state text NOT NULL CHECK(state IN('created','cancelled','dispatching','unknown','open','expired','paid_review','credited','review_required')),
 dispatch_binding text UNIQUE CHECK(dispatch_binding ~ '^[a-f0-9]{32}$'), session_id text UNIQUE, checkout_url text, dispatched_at timestamptz, created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 FOREIGN KEY(merchant_id,offer_id) REFERENCES test_credit_offers(merchant_id,offer_id),
 CHECK((state IN ('created','cancelled'))=(dispatched_at IS NULL)), CHECK((dispatched_at IS NULL)=(dispatch_binding IS NULL)), CHECK(checkout_url IS NULL OR session_id IS NOT NULL)
);
CREATE INDEX test_checkout_owner ON test_checkout_orders(owner_id,requester_id,created_at);
CREATE FUNCTION protect_test_order_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'state'-'session_id'-'checkout_url'-'dispatched_at'-'dispatch_binding') IS DISTINCT FROM (to_jsonb(OLD)-'state'-'session_id'-'checkout_url'-'dispatched_at'-'dispatch_binding')
 OR (OLD.dispatched_at IS NOT NULL AND NEW.dispatched_at IS DISTINCT FROM OLD.dispatched_at)
 OR (OLD.dispatch_binding IS NOT NULL AND NEW.dispatch_binding IS DISTINCT FROM OLD.dispatch_binding)
 OR (OLD.session_id IS NOT NULL AND NEW.session_id IS DISTINCT FROM OLD.session_id)
 THEN RAISE EXCEPTION 'immutable test checkout identity'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER immutable_test_order BEFORE UPDATE OR DELETE ON test_checkout_orders FOR EACH ROW EXECUTE FUNCTION protect_test_order_identity();
CREATE TABLE test_stripe_inbox (
 merchant_id text NOT NULL,event_id text NOT NULL,kind text NOT NULL,session_id text NOT NULL,
 raw_sha256 text NOT NULL CHECK(raw_sha256 ~ '^[a-f0-9]{64}$'),received_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(merchant_id,event_id)
);
CREATE TRIGGER immutable_test_stripe_event BEFORE UPDATE OR DELETE ON test_stripe_inbox FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_stripe_work(
 merchant_id text NOT NULL,event_id text NOT NULL,attempts integer NOT NULL DEFAULT 0 CHECK(attempts>=0),
 next_attempt timestamptz NOT NULL DEFAULT clock_timestamp(),result text CHECK(result IN ('recorded','irrelevant','review_required')),
 PRIMARY KEY(merchant_id,event_id),FOREIGN KEY(merchant_id,event_id) REFERENCES test_stripe_inbox(merchant_id,event_id)
);
CREATE INDEX test_stripe_due ON test_stripe_work(merchant_id,next_attempt,event_id) WHERE result IS NULL;
CREATE TABLE test_credit_fulfillments(
 order_id text PRIMARY KEY REFERENCES test_checkout_orders(order_id),session_id text NOT NULL UNIQUE,payment_intent text NOT NULL UNIQUE,charge_id text NOT NULL UNIQUE,
 owner_id text NOT NULL REFERENCES owners(owner_id),amount_minor bigint NOT NULL CHECK(amount_minor>0),currency text NOT NULL,
 microcredits bigint NOT NULL CHECK(microcredits>0),transaction_id text NOT NULL UNIQUE REFERENCES ledger_transactions(transaction_id),
 recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_test_fulfillment BEFORE UPDATE OR DELETE ON test_credit_fulfillments FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
