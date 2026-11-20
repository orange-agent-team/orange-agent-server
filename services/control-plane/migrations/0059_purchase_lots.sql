-- Existing writers already lock wallets and post immutable purchased:available entries.
-- Attribute those very postings in the same transaction, including from the previous binary.
LOCK TABLE wallets,ledger_transactions,ledger_entries,test_credit_fulfillments IN SHARE ROW EXCLUSIVE MODE;
CREATE TABLE test_purchase_lots(
 order_id text PRIMARY KEY REFERENCES test_credit_fulfillments(order_id),
 owner_id text NOT NULL REFERENCES owners(owner_id), acquired_at timestamptz NOT NULL,
 original_units bigint NOT NULL CHECK(original_units>0), available bigint NOT NULL CHECK(available>=0),
 job_held bigint NOT NULL DEFAULT 0 CHECK(job_held>=0), spent bigint NOT NULL DEFAULT 0 CHECK(spent>=0),
 refund_held bigint NOT NULL DEFAULT 0 CHECK(refund_held>=0), refunded bigint NOT NULL DEFAULT 0 CHECK(refunded>=0),
 CHECK(available::numeric+job_held+spent+refund_held+refunded=original_units)
);
CREATE INDEX open_test_purchase_lots ON test_purchase_lots(owner_id,acquired_at,order_id) WHERE available>0;
CREATE TABLE test_job_lot_allocations(
 job_id text NOT NULL REFERENCES jobs(job_id),order_id text NOT NULL REFERENCES test_purchase_lots(order_id),
 held bigint NOT NULL CHECK(held>0),charged bigint CHECK(charged BETWEEN 0 AND held),
 PRIMARY KEY(job_id,order_id)
);
CREATE FUNCTION protect_test_lot_allocation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.charged IS NOT NULL OR NEW.charged IS NULL OR (to_jsonb(NEW)-'charged') IS DISTINCT FROM (to_jsonb(OLD)-'charged')
 THEN RAISE EXCEPTION 'immutable job purchase allocation'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER immutable_lot_allocation BEFORE UPDATE OR DELETE ON test_job_lot_allocations
FOR EACH ROW EXECUTE FUNCTION protect_test_lot_allocation();

CREATE FUNCTION make_test_purchase_lot(p_order text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO test_purchase_lots(order_id,owner_id,acquired_at,original_units,available)
 SELECT f.order_id,f.owner_id,l.created_at,f.microcredits,f.microcredits FROM test_credit_fulfillments f
 JOIN ledger_transactions l USING(transaction_id) WHERE f.order_id=p_order;
 IF NOT FOUND THEN RAISE EXCEPTION 'missing immutable purchase fact'; END IF;
END $$;
CREATE FUNCTION link_test_purchase_lot() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM make_test_purchase_lot(NEW.order_id); RETURN NEW; END $$;

CREATE FUNCTION allocate_purchase_posting(p_key text,p_owner text,p_delta bigint,p_available_before numeric) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
 target_job text; held bigint; tracked numeric; remaining numeric; quantity bigint; lot record;
BEGIN
 IF NOT EXISTS(SELECT 1 FROM test_purchase_lots WHERE owner_id=p_owner) THEN RETURN; END IF;
 target_job=split_part(p_key,':',2);
 IF split_part(p_key,':',1) NOT IN ('reserve','release','settle') THEN
    IF p_delta<0 AND (SELECT sum(available) FROM test_purchase_lots WHERE owner_id=p_owner)>p_available_before+p_delta THEN
        RAISE EXCEPTION 'unrecognized historic purchased debit requires reconciliation';
    END IF;
    RETURN;
 END IF;
 SELECT held_purchased INTO STRICT held FROM jobs WHERE job_id=target_job AND owner_id=p_owner;
 IF held=0 THEN
    IF p_delta<>0 THEN RAISE EXCEPTION 'unexpected purchased posting'; END IF;
    RETURN;
 END IF;
 IF p_key='reserve:'||target_job THEN
    IF p_delta<>-held THEN RAISE EXCEPTION 'reservation does not match wallet'; END IF;
    SELECT COALESCE(sum(available),0) INTO tracked FROM test_purchase_lots WHERE owner_id=p_owner;
    IF tracked>p_available_before OR p_available_before<held THEN RAISE EXCEPTION 'purchase attribution exceeds wallet'; END IF;
    -- Existing unattributed purchased balances predate these lots and are consumed first.
    remaining=greatest(0,held-(p_available_before-tracked));
    FOR lot IN SELECT * FROM test_purchase_lots WHERE owner_id=p_owner AND available>0 ORDER BY acquired_at,order_id FOR UPDATE LOOP
        EXIT WHEN remaining=0;
        quantity=least(remaining,lot.available)::bigint;
        UPDATE test_purchase_lots SET available=available-quantity,job_held=job_held+quantity WHERE order_id=lot.order_id;
        INSERT INTO test_job_lot_allocations(job_id,order_id,held) VALUES(target_job,lot.order_id,quantity);
        remaining=remaining-quantity;
    END LOOP;
 ELSE
    IF p_delta NOT BETWEEN 0 AND held OR (p_key='release:'||target_job AND p_delta<>held) THEN RAISE EXCEPTION 'invalid purchase release'; END IF;
    SELECT COALESCE(sum(held_units),0) INTO tracked FROM (SELECT a.held AS held_units FROM test_job_lot_allocations a WHERE a.job_id=target_job) h;
    -- A bill spends from the same original reservation. Its remainder returns to the originating lots.
    remaining=greatest(0,(held-p_delta)-(held-tracked));
    FOR lot IN SELECT a.order_id,a.held,a.charged FROM test_job_lot_allocations a JOIN test_purchase_lots l USING(order_id)
         WHERE a.job_id=target_job AND l.owner_id=p_owner ORDER BY l.acquired_at,l.order_id FOR UPDATE OF a,l LOOP
        IF lot.charged IS NOT NULL THEN RAISE EXCEPTION 'job purchase already finalized'; END IF;
        quantity=least(remaining,lot.held)::bigint;
        UPDATE test_purchase_lots SET job_held=job_held-lot.held,spent=spent+quantity,available=available+(lot.held-quantity) WHERE order_id=lot.order_id;
        UPDATE test_job_lot_allocations SET charged=quantity WHERE job_id=target_job AND order_id=lot.order_id;
        remaining=remaining-quantity;
    END LOOP;
 END IF;
 IF remaining<>0 THEN RAISE EXCEPTION 'incomplete purchase attribution'; END IF;
END $$;

-- Reconstruct exact historic available-before values from the immutable ledger and current wallet.
-- Refuse an ambiguous historic record instead of silently making previously spent credits refundable.
DO $$
DECLARE x record;
BEGIN
 FOR x IN
   SELECT l.owner_id,l.business_key,l.created_at,l.transaction_id,e.amount,
          w.purchased-sum(e.amount::numeric) OVER(PARTITION BY l.owner_id ORDER BY l.created_at,l.transaction_id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) AS available_before
   FROM ledger_transactions l JOIN ledger_entries e USING(transaction_id) JOIN wallets w USING(owner_id)
   WHERE e.account=l.owner_id||':purchased:available' AND EXISTS(SELECT 1 FROM test_credit_fulfillments f WHERE f.owner_id=l.owner_id)
   ORDER BY l.owner_id,l.created_at,l.transaction_id
 LOOP
   IF x.business_key LIKE 'test-checkout:%' THEN
     PERFORM make_test_purchase_lot(substr(x.business_key,length('test-checkout:')+1));
   ELSE PERFORM allocate_purchase_posting(x.business_key,x.owner_id,x.amount,x.available_before); END IF;
 END LOOP;
 IF EXISTS(SELECT 1 FROM test_credit_fulfillments f LEFT JOIN test_purchase_lots l USING(order_id) WHERE l.order_id IS NULL)
 THEN RAISE EXCEPTION 'missing original purchased balance posting'; END IF;
 IF EXISTS(SELECT l.owner_id FROM test_purchase_lots l JOIN wallets w USING(owner_id) GROUP BY l.owner_id,w.purchased,w.reserved_purchased
     HAVING sum(l.available)>w.purchased OR sum(l.job_held)>w.reserved_purchased) THEN RAISE EXCEPTION 'historical lots exceed wallet buckets'; END IF;
END $$;
CREATE TRIGGER create_purchase_lot AFTER INSERT ON test_credit_fulfillments FOR EACH ROW EXECUTE FUNCTION link_test_purchase_lot();
CREATE FUNCTION link_test_job_purchase() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE who text; business text; after_available bigint;
BEGIN
 SELECT owner_id,business_key INTO STRICT who,business FROM ledger_transactions WHERE transaction_id=NEW.transaction_id;
 IF NEW.account=who||':purchased:available' AND split_part(business,':',1) IN('reserve','settle','release') THEN
    SELECT purchased INTO STRICT after_available FROM wallets WHERE owner_id=who FOR UPDATE;
    PERFORM allocate_purchase_posting(business,who,NEW.amount,after_available::numeric-NEW.amount);
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER test_job_purchase AFTER INSERT ON ledger_entries FOR EACH ROW WHEN (NEW.account LIKE '%:purchased:available') EXECUTE FUNCTION link_test_job_purchase();
CREATE FUNCTION protect_purchase_lot_origin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR ROW(NEW.order_id,NEW.owner_id,NEW.original_units,NEW.acquired_at) IS DISTINCT FROM ROW(OLD.order_id,OLD.owner_id,OLD.original_units,OLD.acquired_at)
 THEN RAISE EXCEPTION 'immutable purchased lot origin'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER immutable_purchase_origin BEFORE UPDATE OR DELETE ON test_purchase_lots FOR EACH ROW EXECUTE FUNCTION protect_purchase_lot_origin();
