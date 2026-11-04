-- First implemented vertical slice. No production payments or GPU execution.
CREATE TABLE owners (
    owner_id TEXT PRIMARY KEY,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','restricted','closed'))
);
CREATE TABLE wallets (
    owner_id TEXT PRIMARY KEY REFERENCES owners(owner_id),
    gift BIGINT NOT NULL DEFAULT 0 CHECK (gift >= 0),
    earned BIGINT NOT NULL DEFAULT 0 CHECK (earned >= 0),
    purchased BIGINT NOT NULL DEFAULT 0 CHECK (purchased >= 0),
    reserved_gift BIGINT NOT NULL DEFAULT 0 CHECK (reserved_gift >= 0),
    reserved_earned BIGINT NOT NULL DEFAULT 0 CHECK (reserved_earned >= 0),
    reserved_purchased BIGINT NOT NULL DEFAULT 0 CHECK (reserved_purchased >= 0)
);
CREATE TABLE budget_roots (
    root_budget_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL REFERENCES owners(owner_id),
    scope_id TEXT NOT NULL,
    limit_amount BIGINT NOT NULL CHECK (limit_amount >= 0),
    spent BIGINT NOT NULL DEFAULT 0 CHECK (spent >= 0),
    reserved BIGINT NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','closing','closed')),
    CHECK (spent::numeric + reserved::numeric <= limit_amount::numeric),
    UNIQUE(owner_id, scope_id)
);
CREATE TABLE quotes (
    quote_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL REFERENCES owners(owner_id),
    root_budget_id TEXT NOT NULL REFERENCES budget_roots(root_budget_id),
    invocation_id TEXT NOT NULL,
    projection_digest TEXT NOT NULL,
    price_version TEXT NOT NULL,
    max_amount BIGINT NOT NULL CHECK (max_amount >= 0),
    expires_at TIMESTAMPTZ NOT NULL,
    UNIQUE(owner_id, root_budget_id, invocation_id, quote_id)
);
CREATE TABLE jobs (
    job_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL REFERENCES owners(owner_id),
    root_budget_id TEXT NOT NULL REFERENCES budget_roots(root_budget_id),
    invocation_id TEXT NOT NULL,
    purpose TEXT NOT NULL,
    quote_id TEXT NOT NULL REFERENCES quotes(quote_id),
    request_digest TEXT NOT NULL,
    execution_state TEXT NOT NULL CHECK(execution_state IN ('queued','running','cancelling','succeeded','failed','cancelled')),
    settlement_state TEXT NOT NULL DEFAULT 'pending' CHECK(settlement_state IN ('pending','settled','reversed')),
    reserved BIGINT NOT NULL CHECK(reserved >= 0),
    held_gift BIGINT NOT NULL CHECK(held_gift >= 0),
    held_earned BIGINT NOT NULL CHECK(held_earned >= 0),
    held_purchased BIGINT NOT NULL CHECK(held_purchased >= 0),
    charge BIGINT CHECK(charge >= 0 AND charge <= reserved),
    CHECK(held_gift::numeric + held_earned::numeric + held_purchased::numeric = reserved::numeric),
    UNIQUE(owner_id, invocation_id)
);
CREATE TABLE idempotency (
    owner_id TEXT NOT NULL REFERENCES owners(owner_id),
    operation TEXT NOT NULL,
    key TEXT NOT NULL,
    digest TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    PRIMARY KEY(owner_id, operation, key)
);
CREATE TABLE ledger_transactions (
    transaction_id TEXT PRIMARY KEY,
    business_key TEXT NOT NULL UNIQUE,
    owner_id TEXT NOT NULL REFERENCES owners(owner_id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE ledger_entries (
    transaction_id TEXT NOT NULL REFERENCES ledger_transactions(transaction_id),
    account TEXT NOT NULL,
    amount BIGINT NOT NULL,
    PRIMARY KEY(transaction_id, account)
);
CREATE FUNCTION forbid_ledger_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'ledger facts are immutable'; END $$;
CREATE TRIGGER ledger_transactions_immutable BEFORE UPDATE OR DELETE ON ledger_transactions
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE TRIGGER ledger_entries_immutable BEFORE UPDATE OR DELETE ON ledger_entries
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
CREATE FUNCTION verify_ledger_balance() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF (SELECT COALESCE(SUM(amount::numeric),0) FROM ledger_entries WHERE transaction_id=NEW.transaction_id) <> 0 THEN
  RAISE EXCEPTION 'unbalanced ledger transaction %', NEW.transaction_id;
 END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER ledger_balance AFTER INSERT ON ledger_entries
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION verify_ledger_balance();
CREATE TABLE outbox (
    event_id BIGSERIAL PRIMARY KEY,
    owner_id TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    delivered_at TIMESTAMPTZ
);
