-- Amounts on merchant statements are signed minor units in their own settlement currency.
CREATE TABLE test_statement_runs(
    run_id text PRIMARY KEY, merchant_id text NOT NULL,
    from_unix bigint NOT NULL, until_unix bigint NOT NULL CHECK(until_unix>from_unix AND until_unix-from_unix<=172800),
    completed_at timestamptz, report jsonb,
    CHECK((completed_at IS NULL)=(report IS NULL))
);
CREATE INDEX test_statement_completed ON test_statement_runs(merchant_id,until_unix DESC) WHERE completed_at IS NOT NULL;
CREATE TABLE test_statement_sync(
    merchant_id text PRIMARY KEY, activation_day bigint NOT NULL DEFAULT (floor(extract(epoch FROM clock_timestamp())/86400)::bigint*86400), high_water bigint, run_id text UNIQUE REFERENCES test_statement_runs,
    phase text NOT NULL DEFAULT 'external' CHECK(phase IN ('external','local','jobs','holds')),
    page_cursor text NOT NULL DEFAULT '', generation bigint NOT NULL DEFAULT 0,
    claim_until timestamptz, next_check timestamptz NOT NULL DEFAULT clock_timestamp(),
    failures integer NOT NULL DEFAULT 0, last_error text
);
CREATE TABLE test_statement_lines(
    merchant_id text NOT NULL, line_id text NOT NULL, source_id text, kind text NOT NULL, category text NOT NULL,
    created_unix bigint NOT NULL, currency text NOT NULL CHECK(currency ~ '^[a-z]{3}$'),
    gross bigint NOT NULL, fee bigint NOT NULL, net bigint NOT NULL,
    PRIMARY KEY(merchant_id,line_id), CHECK(net::numeric=gross::numeric-fee::numeric)
);
CREATE INDEX test_statement_source ON test_statement_lines(merchant_id,source_id,kind);
CREATE TRIGGER immutable_statement_line BEFORE UPDATE OR DELETE ON test_statement_lines FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_statement_run_lines(
    run_id text NOT NULL REFERENCES test_statement_runs, merchant_id text NOT NULL, line_id text NOT NULL,
    created_unix bigint NOT NULL, currency text NOT NULL, category text NOT NULL, gross bigint NOT NULL, fee bigint NOT NULL, net bigint NOT NULL,
    provider_status text NOT NULL CHECK(provider_status IN('pending','available')),
    PRIMARY KEY(run_id,line_id), FOREIGN KEY(merchant_id,line_id) REFERENCES test_statement_lines
);
CREATE TRIGGER immutable_statement_seen BEFORE UPDATE OR DELETE ON test_statement_run_lines FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_statement_issues(
    merchant_id text NOT NULL, issue_key text NOT NULL, kind text NOT NULL, business_ref text NOT NULL,
    owner_id text REFERENCES owners, case_id text UNIQUE REFERENCES reconciliation_cases,
    PRIMARY KEY(merchant_id,issue_key)
);
CREATE TABLE test_statement_issue_evidence(
    run_id text NOT NULL REFERENCES test_statement_runs, merchant_id text NOT NULL, issue_key text NOT NULL, evidence_digest text NOT NULL,
    detail jsonb NOT NULL, PRIMARY KEY(run_id,issue_key,evidence_digest),
    FOREIGN KEY(merchant_id,issue_key) REFERENCES test_statement_issues
);
CREATE TRIGGER immutable_statement_evidence BEFORE UPDATE OR DELETE ON test_statement_issue_evidence FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION seal_test_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.completed_at IS NOT NULL OR NEW.completed_at IS NULL OR
    (to_jsonb(NEW)-'completed_at'-'report') IS DISTINCT FROM (to_jsonb(OLD)-'completed_at'-'report') THEN
   RAISE EXCEPTION 'immutable statement report';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER immutable_statement_report BEFORE UPDATE OR DELETE ON test_statement_runs FOR EACH ROW EXECUTE FUNCTION seal_test_statement();

CREATE VIEW test_statement_obligations AS
 SELECT o.merchant_id,f.owner_id,f.order_id AS business_id,f.charge_id AS source_id,'purchase'::text AS flow,
        f.amount_minor AS gross,f.currency,f.microcredits AS units,f.transaction_id,f.recorded_at
   FROM test_credit_fulfillments f JOIN test_checkout_orders o USING(order_id)
 UNION ALL
 SELECT o.merchant_id,r.owner_id,r.refund_id,r.provider_refund_id,'refund',-r.amount_minor,r.currency,-r.held_units,c.transaction_id,c.recorded_at
   FROM test_refund_orders r JOIN test_refund_receipts c USING(refund_id) JOIN test_checkout_orders o USING(order_id)
  WHERE c.outcome='succeeded';
CREATE INDEX test_credit_recorded ON test_credit_fulfillments(recorded_at,order_id);
CREATE INDEX test_refund_recorded ON test_refund_receipts(recorded_at,refund_id);
ALTER TABLE test_statement_runs ADD COLUMN sequence_id bigint GENERATED ALWAYS AS IDENTITY UNIQUE;
CREATE INDEX test_statement_issue_observation ON test_statement_issue_evidence(merchant_id,issue_key,run_id);

CREATE VIEW test_statement_pending_items AS
 SELECT merchant_id,owner_id,order_id AS business_id,'stale_checkout'::text AS kind,state,dispatched_at AS opened_at,
        NULL::bigint AS held_units FROM test_checkout_orders
 WHERE state IN ('dispatching','unknown','paid_review','review_required')
 UNION ALL
 SELECT o.merchant_id,r.owner_id,r.refund_id,'stale_refund_hold',r.state,r.created_at,r.held_units
 FROM test_refund_orders r JOIN test_checkout_orders o USING(order_id)
 WHERE r.state NOT IN ('succeeded','failed','cancelled')
 UNION ALL
 SELECT o.merchant_id,j.owner_id,j.job_id,'stale_job_hold',j.execution_state,t.created_at,sum(a.held)::bigint
 FROM test_job_lot_allocations a JOIN jobs j USING(job_id) JOIN test_checkout_orders o USING(order_id)
 JOIN ledger_transactions t ON t.business_key='reserve:'||j.job_id
 WHERE a.charged IS NULL AND j.settlement_state='pending'
 GROUP BY o.merchant_id,j.owner_id,j.job_id,j.execution_state,t.created_at;
CREATE TABLE test_statement_run_obligations(
 run_id text NOT NULL REFERENCES test_statement_runs,flow text NOT NULL CHECK(flow IN('purchase','refund')),
 business_id text NOT NULL,owner_id text NOT NULL REFERENCES owners,source_id text NOT NULL,
 currency text NOT NULL,gross bigint NOT NULL,units bigint NOT NULL,transaction_id text NOT NULL REFERENCES ledger_transactions,
 recorded_at timestamptz NOT NULL,PRIMARY KEY(run_id,flow,business_id)
);
CREATE TRIGGER immutable_scanned_obligation BEFORE UPDATE OR DELETE ON test_statement_run_obligations FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE test_statement_failures(
 run_id text NOT NULL REFERENCES test_statement_runs,generation bigint NOT NULL,phase text NOT NULL,page_cursor text NOT NULL,
 reason text NOT NULL,recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),PRIMARY KEY(run_id,generation)
);
CREATE TRIGGER immutable_statement_failure BEFORE UPDATE OR DELETE ON test_statement_failures FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX statement_business_charge ON test_job_lot_allocations(order_id,job_id);

CREATE TABLE test_statement_job_checks(
 run_id text NOT NULL REFERENCES test_statement_runs, job_id text NOT NULL REFERENCES jobs, owner_id text NOT NULL REFERENCES owners,
 expected_provider text NOT NULL REFERENCES owners, transaction_id text REFERENCES ledger_transactions, charge bigint,
 verified boolean NOT NULL, PRIMARY KEY(run_id,job_id), CHECK ((transaction_id IS NULL)=(charge IS NULL)),
 CHECK (NOT verified OR transaction_id IS NOT NULL)
);
CREATE TRIGGER immutable_statement_job_check BEFORE UPDATE OR DELETE ON test_statement_job_checks FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
