# Personal account API

The current authenticated session selects the personal owner for wallet reads, budget creation and budget pagination. Requests cannot select another owner or fall back to a development bearer identity. Team budgets and their explicit member spend grants remain separate work.

GET /api/v1/account/wallet returns canonical decimal strings for available gift/earned/purchased, their reserved buckets and pending_earned. Pending earnings are not spendable. A read is a point-in-time fact; submitting a Job still rechecks current authority and freezes funds atomically.

GET/POST /api/v1/account/budgets and GET /api/v1/account/budgets/{root} use the current user-session transaction. Creation validates Origin/CSRF and a stable Idempotency-Key, then rechecks expiry before commit. Same request returns its original root; changed contents conflict. A budget creates a spending limit only, without minting credits or reserving wallet funds. Same-owner creation is serialized without upgrading the shared owner lock held by authentication.

Lists use root_budget_id pagination, default50/max100; they are not a frozen snapshot across pages. Cross-owner reads return404. Revoked/expired sessions cannot bypass authorization using prior keys. Tests cover exact amounts beyond JS safe integers, two-session concurrency, unchanged zero balances, pagination, CSRF/extra fields/owner rejection and expiry rollback after a real database delay.

No recharge, payout, balance mutation, team budget governance, budget adjustment/closure, payment or production financial certification. Source-bound acceptance, strict Clippy and related regressions passed as PERSONAL-ACCOUNT-001; original parent tasks and the complete roadmap remain unfinished. Run `python3 scripts/check-user-accounts.py` for the scoped acceptance tests.
