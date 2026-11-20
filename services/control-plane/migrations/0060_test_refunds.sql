CREATE TABLE test_refund_orders(
 refund_id text PRIMARY KEY,order_id text NOT NULL REFERENCES test_purchase_lots(order_id),owner_id text NOT NULL REFERENCES owners(owner_id),
 requester_id text NOT NULL REFERENCES users(user_id),case_id text NOT NULL UNIQUE REFERENCES reconciliation_cases(case_id),
 currency text NOT NULL,amount_minor bigint NOT NULL CHECK(amount_minor>0),held_units bigint NOT NULL CHECK(held_units>0),
 prior_minor bigint NOT NULL CHECK(prior_minor>=0),target_version text NOT NULL UNIQUE CHECK(target_version ~ '^[a-f0-9]{64}$'),
 state text NOT NULL CHECK(state IN('held','authorized','approval_lost','dispatching','unknown','pending','succeeded','failed','cancelled','review_required')),
 dispatch_binding text UNIQUE CHECK(dispatch_binding ~ '^[a-f0-9]{32}$'),provider_refund_id text UNIQUE,scan_after text,checks integer NOT NULL DEFAULT 0 CHECK(checks>=0),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),next_check timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE UNIQUE INDEX one_unresolved_test_refund ON test_refund_orders(order_id) WHERE state NOT IN ('succeeded','failed','cancelled');
CREATE INDEX due_test_refund ON test_refund_orders(next_check) WHERE state IN ('authorized','pending','unknown','dispatching');
CREATE TABLE test_refund_authorizations(
 refund_id text PRIMARY KEY REFERENCES test_refund_orders(refund_id),action_id text NOT NULL UNIQUE REFERENCES admin_action_control(action_id),
 maker_id text NOT NULL REFERENCES admin_principals(principal_id),checker_id text NOT NULL REFERENCES admin_principals(principal_id),
 executor_id text NOT NULL REFERENCES admin_principals(principal_id),scope_kind text NOT NULL CHECK(scope_kind IN ('platform','tenant')),scope_id text NOT NULL,created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK(maker_id<>checker_id)
);
CREATE TRIGGER immutable_test_refund_authorization BEFORE UPDATE OR DELETE ON test_refund_authorizations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_refund_receipts(
 refund_id text PRIMARY KEY REFERENCES test_refund_orders(refund_id),
 outcome text NOT NULL CHECK(outcome IN ('succeeded','failed','cancelled')),
 transaction_id text NOT NULL UNIQUE REFERENCES ledger_transactions(transaction_id),recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_test_refund_receipt BEFORE UPDATE OR DELETE ON test_refund_receipts FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION protect_test_refund_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'state'-'dispatch_binding'-'provider_refund_id'-'next_check'-'scan_after'-'checks') IS DISTINCT FROM (to_jsonb(OLD)-'state'-'dispatch_binding'-'provider_refund_id'-'next_check'-'scan_after'-'checks')
 OR (OLD.dispatch_binding IS NOT NULL AND NEW.dispatch_binding IS DISTINCT FROM OLD.dispatch_binding)
 OR (OLD.provider_refund_id IS NOT NULL AND NEW.provider_refund_id IS DISTINCT FROM OLD.provider_refund_id)
 OR OLD.state IN ('succeeded','failed','cancelled') THEN RAISE EXCEPTION 'immutable test refund terms'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER protected_test_refund BEFORE UPDATE OR DELETE ON test_refund_orders FOR EACH ROW EXECUTE FUNCTION protect_test_refund_terms();
CREATE TABLE test_stripe_refund_events(
 merchant_id text NOT NULL,event_id text NOT NULL,kind text NOT NULL,refund_id text NOT NULL REFERENCES test_refund_orders(refund_id),
 provider_refund_id text NOT NULL,raw_sha256 text NOT NULL CHECK(raw_sha256 ~ '^[a-f0-9]{64}$'),received_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(merchant_id,event_id)
);
CREATE INDEX test_refund_event_lookup ON test_stripe_refund_events(refund_id,received_at,event_id);
CREATE TRIGGER immutable_refund_event BEFORE UPDATE OR DELETE ON test_stripe_refund_events FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
-- Stable finance history cursor. Existing checkout orders get a sequence during the upgrade.
ALTER TABLE test_checkout_orders ADD COLUMN order_sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE;
CREATE INDEX test_checkout_history ON test_checkout_orders(merchant_id,owner_id,requester_id,order_sequence DESC);
