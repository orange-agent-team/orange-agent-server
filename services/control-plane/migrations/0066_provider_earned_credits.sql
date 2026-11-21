-- Internal development credits. An original job has one pending entitlement and at most one release posting.
CREATE FUNCTION provider_credit_now() RETURNS timestamptz LANGUAGE sql VOLATILE AS $$ SELECT clock_timestamp() $$;
CREATE FUNCTION provider_related(p_buyer text,p_supplier text,p_submitter text) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT p_buyer=p_supplier OR EXISTS(SELECT 1 FROM users u WHERE u.personal_owner_id=p_supplier AND u.user_id=p_submitter)
      OR EXISTS(SELECT 1 FROM memberships m JOIN users u USING(user_id) WHERE m.active AND
         ((m.organization_id=p_buyer AND u.personal_owner_id=p_supplier) OR (m.organization_id=p_supplier AND u.personal_owner_id=p_buyer)))
$$;
CREATE TABLE provider_earning_origins(
    job_id text PRIMARY KEY REFERENCES job_settlements,source_transaction text NOT NULL UNIQUE REFERENCES ledger_transactions,
    buyer_owner text NOT NULL REFERENCES owners,provider_owner text NOT NULL REFERENCES owners,
    units bigint NOT NULL CHECK(units>0),settled_at timestamptz NOT NULL,eligible_at timestamptz NOT NULL,
    relation_at_posting boolean, CHECK(eligible_at=settled_at+interval '24 hours')
);
CREATE TRIGGER immutable_earning_origin BEFORE UPDATE OR DELETE ON provider_earning_origins FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE provider_earning_cases(
    job_id text PRIMARY KEY REFERENCES provider_earning_origins,case_id text UNIQUE NOT NULL REFERENCES reconciliation_cases
);
CREATE TRIGGER immutable_earning_case BEFORE UPDATE OR DELETE ON provider_earning_cases FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TABLE provider_earning_releases(
    job_id text PRIMARY KEY REFERENCES provider_earning_origins,transaction_id text NOT NULL UNIQUE REFERENCES ledger_transactions,
    action_id text NOT NULL UNIQUE REFERENCES admin_actions(action_id),case_id text NOT NULL UNIQUE REFERENCES reconciliation_cases,
    released_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER immutable_earning_release BEFORE UPDATE OR DELETE ON provider_earning_releases FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE INDEX provider_earnings_mature ON provider_earning_origins(eligible_at,job_id);
-- No prior version captured membership at settlement. Keep that uncertainty visible to review.
INSERT INTO provider_earning_origins
 SELECT s.job_id,s.transaction_id,j.owner_id,s.provider_owner_id,s.provider_pending,s.settled_at,s.settled_at+interval '24 hours',
 CASE WHEN EXISTS(SELECT 1 FROM organizations t WHERE t.organization_id IN(j.owner_id,s.provider_owner_id)) THEN NULL
 ELSE provider_related(j.owner_id,s.provider_owner_id,j.submitting_user_id) END
 FROM job_settlements s JOIN jobs j USING(job_id) WHERE s.provider_pending>0;
CREATE FUNCTION register_provider_earning() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.provider_pending>0 THEN
  INSERT INTO provider_earning_origins SELECT NEW.job_id,NEW.transaction_id,j.owner_id,NEW.provider_owner_id,NEW.provider_pending,NEW.settled_at,
      NEW.settled_at+interval '24 hours',provider_related(j.owner_id,NEW.provider_owner_id,j.submitting_user_id)
      FROM jobs j WHERE j.job_id=NEW.job_id;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER register_provider_income AFTER INSERT ON job_settlements FOR EACH ROW EXECUTE FUNCTION register_provider_earning();
CREATE VIEW provider_earning_status AS
 SELECT e.*,c.case_id,r.transaction_id AS release_transaction,CASE
 WHEN r.job_id IS NOT NULL THEN 'released'
 WHEN e.eligible_at>provider_credit_now() THEN 'holding_period'
 WHEN e.relation_at_posting IS NULL THEN 'historical_affiliation_unknown'
 WHEN e.relation_at_posting OR provider_related(e.buyer_owner,e.provider_owner,j.submitting_user_id) THEN 'related_trade'
 WHEN EXISTS(SELECT 1 FROM owners o WHERE o.owner_id IN (e.buyer_owner,e.provider_owner) AND o.state<>'active') THEN 'restricted_account'
 WHEN EXISTS(SELECT 1 FROM test_job_lot_allocations a JOIN test_dispute_work d USING(order_id) WHERE a.job_id=e.job_id AND a.charged>0 AND d.blocks_spend) THEN 'disputed_purchase'
 WHEN EXISTS(SELECT 1 FROM test_statement_issues i JOIN reconciliation_cases k USING(case_id) WHERE i.business_ref=e.job_id AND i.owner_id=e.buyer_owner AND i.kind IN ('missing_job_settlement','job_posting_mismatch','stale_job_hold') AND k.state<>'resolved') THEN 'statement_issue'
 WHEN EXISTS(SELECT 1 FROM quote_execution_plans p JOIN nodes n ON n.node_id=p.target_node_id WHERE p.quote_id=j.quote_id AND n.state IN('quarantined','revoked')) THEN 'provider_restricted'
 WHEN NOT EXISTS(SELECT 1 FROM metering_receipts m JOIN ledger_entries l USING(transaction_id) WHERE m.job_id=e.job_id AND m.state='posted'
    AND m.transaction_id=e.source_transaction AND l.account=e.provider_owner||':earned:pending' AND l.amount=e.units) THEN 'unverified_posting'
 ELSE 'ready_for_review' END AS disposition
 FROM provider_earning_origins e JOIN jobs j USING(job_id) LEFT JOIN provider_earning_cases c USING(job_id) LEFT JOIN provider_earning_releases r USING(job_id);

CREATE INDEX provider_earning_by_owner ON provider_earning_origins(provider_owner,settled_at DESC,job_id DESC);
CREATE FUNCTION fence_earning_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.kind IN('missing_job_settlement','job_posting_mismatch','stale_job_hold') THEN
  PERFORM job_id FROM provider_earning_origins WHERE job_id=NEW.business_ref FOR UPDATE;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER fence_earning_discrepancy BEFORE INSERT OR UPDATE OF case_id ON test_statement_issues
FOR EACH ROW EXECUTE FUNCTION fence_earning_statement();
