-- One statement and one MVCC snapshot. Financial facts and wallet materializations are read only.
WITH buckets(suffix, wallet_field) AS (
    VALUES ('gift:available','gift'), ('earned:available','earned'),
           ('purchased:available','purchased'), ('gift:reserved','reserved_gift'),
           ('earned:reserved','reserved_earned'), ('purchased:reserved','reserved_purchased'),
           ('purchased:refund-held','reserved_purchased'), ('earned:pending','pending_earned')
), account_totals AS (
    SELECT account, sum(amount::numeric) AS units FROM ledger_entries GROUP BY account
), posted AS (
    SELECT left(a.account, length(a.account)-length(b.suffix)-1) AS owner_id,
           b.wallet_field, sum(a.units) AS units
    FROM account_totals a
    JOIN buckets b ON right(a.account, length(b.suffix)+1) = ':' || b.suffix
    GROUP BY 1,2
), current_balances AS (
    SELECT w.owner_id,b.wallet_field,b.units
    FROM wallets w
    CROSS JOIN LATERAL (VALUES
        ('gift',w.gift), ('earned',w.earned), ('purchased',w.purchased),
        ('reserved_gift',w.reserved_gift), ('reserved_earned',w.reserved_earned),
        ('reserved_purchased',w.reserved_purchased), ('pending_earned',w.pending_earned)
    ) b(wallet_field,units)
), differences AS (
    SELECT coalesce(c.owner_id,p.owner_id) AS owner_id,
           coalesce(c.wallet_field,p.wallet_field) AS field,
           c.owner_id IS NULL AS missing_wallet
    FROM current_balances c FULL JOIN posted p USING(owner_id,wallet_field)
    WHERE c.owner_id IS NULL OR c.units IS DISTINCT FROM coalesce(p.units,0)
), unbalanced AS (
    SELECT transaction_id FROM ledger_entries GROUP BY transaction_id HAVING sum(amount::numeric) <> 0
)
SELECT json_build_object(
    'version',1,
    'wallets_checked',(SELECT count(*) FROM wallets),
    'transactions_checked',(SELECT count(*) FROM ledger_transactions),
    'affected_owners',(SELECT count(DISTINCT owner_id) FROM differences),
    'missing_wallets',(SELECT count(DISTINCT owner_id) FROM differences WHERE missing_wallet),
    'mismatches_by_field',coalesce((SELECT json_object_agg(field,total) FROM (
        SELECT field,count(*) AS total FROM differences GROUP BY field
    ) counts),'{}'::json),
    'unbalanced_transactions',(SELECT count(*) FROM unbalanced)
);
