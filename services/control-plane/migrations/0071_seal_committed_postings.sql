-- A balanced addition can still change a past transaction. Accept new lines
-- only while the transaction's own writer is assembling its original group.
CREATE SCHEMA orange_ledger_internal;
REVOKE ALL ON SCHEMA orange_ledger_internal FROM PUBLIC;

CREATE TABLE orange_ledger_internal.open_posting_groups (
    transaction_id text PRIMARY KEY,
    writer_xid xid8 NOT NULL
);
REVOKE ALL ON orange_ledger_internal.open_posting_groups FROM PUBLIC;

CREATE FUNCTION orange_ledger_internal.open_group() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    INSERT INTO orange_ledger_internal.open_posting_groups(transaction_id,writer_xid)
        VALUES(NEW.transaction_id,pg_current_xact_id());
    RETURN NEW;
END $$;
CREATE TRIGGER open_original_posting_group AFTER INSERT ON ledger_transactions
FOR EACH ROW EXECUTE FUNCTION orange_ledger_internal.open_group();

CREATE FUNCTION orange_ledger_internal.close_group() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    DELETE FROM orange_ledger_internal.open_posting_groups
        WHERE transaction_id=NEW.transaction_id AND writer_xid=NEW.writer_xid;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'missing posting group at close' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER close_original_posting_group AFTER INSERT ON orange_ledger_internal.open_posting_groups
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION orange_ledger_internal.close_group();

CREATE FUNCTION orange_ledger_internal.require_open_group() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM orange_ledger_internal.open_posting_groups
        WHERE transaction_id=NEW.transaction_id AND writer_xid=pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION 'ledger postings belong to their original transaction'
            USING ERRCODE='23514', CONSTRAINT='ledger_postings_sealed';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER require_original_posting_group BEFORE INSERT ON ledger_entries
FOR EACH ROW EXECUTE FUNCTION orange_ledger_internal.require_open_group();

REVOKE ALL ON FUNCTION orange_ledger_internal.open_group(), orange_ledger_internal.close_group(),
    orange_ledger_internal.require_open_group() FROM PUBLIC;
