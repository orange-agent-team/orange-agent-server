# Original submission withdrawal

POST /api/v1/owners/{owner}/invocations/{invocation}/withdraw accepts the exact original SubmitTextJob body. Current user, CSRF and owner/membership authority are checked again on every replay. The idempotency key is scoped to current user/owner/Invocation; changing the original body conflicts. This is not a generic ability to cancel another user's quote.

User submission and withdrawal share an owner/Invocation transaction lock before budget/wallet locks. Both remain SERIALIZABLE: an old snapshot after waiting on the lock must fail/retry rather than miss a committed barrier. Current submission still enforces all existing authorization, quote, budget, wallet and original-request checks.

When no original Job has been accepted, an immutable invocation_withdrawals record and durable event commit together. A later same-user submission of that Invocation is refused, even with another submit key. Receipt prevented=true has a receipt_id and no Job, usage, refund or device-cleanup claim. Personal owner identity proves a personal quote; an organization quote also requires the original quote_metering.requester_id.

If a Job already exists, its original submitter and complete request digest must match. The read transaction accepts no separate stop command; the existing cancel_for_user transaction makes the actual cancellation decision. The response has prevented=false and the resulting Job. Running work retains its hold pending real cleanup; completed delivery and its charge are retained. A crash before cancellation is an unconfirmed request, recovered through the same original endpoint, not a claimed accepted stop.

PG0028 has been applied to the isolated test database and the dedicated development database; its bytes are frozen. This is not a production migration certificate. The barrier cannot be modified/deleted. Event failure or expiry before commit rolls back the complete decision. Existing Jobs and normal cancellation retain their prior semantics.

The formal checkout passes11 router/database cases and strict Clippy, including running cancellation with separate declared cleanup. Client receipt handling and complete related regressions are still in progress. Actual model/GPU/payment certification remains separate; no full-roadmap completion is claimed.
