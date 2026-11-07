-- A current user can fence their original not-yet-accepted submission. This is
-- not a resource cleanup receipt or a fabricated zero-usage settlement.
CREATE TABLE invocation_withdrawals(
 owner_id TEXT NOT NULL REFERENCES owners(owner_id),
 user_id TEXT NOT NULL REFERENCES users(user_id),
 invocation_id TEXT NOT NULL,
 root_budget_id TEXT NOT NULL REFERENCES budget_roots(root_budget_id),
 quote_id TEXT NOT NULL REFERENCES quotes(quote_id),
 request_key TEXT NOT NULL,
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 receipt_id TEXT NOT NULL UNIQUE,
 created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(owner_id,user_id,invocation_id)
);
CREATE TRIGGER invocation_withdrawal_immutable BEFORE UPDATE OR DELETE ON invocation_withdrawals
FOR EACH ROW EXECUTE FUNCTION forbid_ledger_mutation();
