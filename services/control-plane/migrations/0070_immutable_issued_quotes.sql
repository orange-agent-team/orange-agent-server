-- The quote identifier is part of the job idempotency digest. Its financial
-- terms must continue to identify the same reservation and execution request.
CREATE FUNCTION orange_protect_quote_terms() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF ROW(NEW.quote_id, NEW.owner_id, NEW.root_budget_id, NEW.invocation_id,
           NEW.projection_digest, NEW.price_version, NEW.max_amount, NEW.expires_at)
       IS DISTINCT FROM
       ROW(OLD.quote_id, OLD.owner_id, OLD.root_budget_id, OLD.invocation_id,
           OLD.projection_digest, OLD.price_version, OLD.max_amount, OLD.expires_at) THEN
        RAISE EXCEPTION 'issued quote terms are immutable'
            USING ERRCODE = '23514', CONSTRAINT = 'immutable_issued_quote_terms';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER immutable_issued_quote_terms BEFORE UPDATE ON quotes
FOR EACH ROW EXECUTE FUNCTION orange_protect_quote_terms();
