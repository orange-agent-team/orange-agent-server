use super::*;
use orange_protocol::pricing::{FundingAccount, FundingStatus, QuoteFundingView};
use orange_protocol::team::TeamRole;

impl ArtifactStore {
    pub(super) async fn funding(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        quote: &str,
    ) -> Result<QuoteFundingView, ArtifactError> {
        if !valid_id(quote) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let role = authority::scope(&mut tx, actor, owner, false).await?;
        let root: String=sqlx::query_scalar("SELECT q.root_budget_id FROM quotes q WHERE q.quote_id=$1 AND q.owner_id=$2
           AND (EXISTS(SELECT 1 FROM quote_metering m WHERE m.quote_id=q.quote_id AND m.requester_id=$3 AND m.owner_id=q.owner_id)
           OR EXISTS(SELECT 1 FROM transcription_quote_terms t WHERE t.quote_id=q.quote_id AND t.requester_id=$3 AND t.owner_id=q.owner_id))")
           .bind(quote).bind(owner).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        crate::store::check_budget_actor(&mut tx, &actor.user, owner, &root)
            .await
            .map_err(|e| match e {
                crate::store::StoreError::Database(db) => ArtifactError::Database(db),
                _ => ArtifactError::Forbidden,
            })?;
        // One snapshot for the quote, owner wallet, budget, dispute status and database time.
        let r=sqlx::query("SELECT q.price_version,q.max_amount,b.limit_amount,b.spent,b.reserved,
            b.limit_amount-b.spent-b.reserved AS remaining,b.state='active' AS open,available::text AS wallet,available>=q.max_amount AS covered,
            q.max_amount>0 AND EXISTS(SELECT 1 FROM test_dispute_work d WHERE d.owner_id=q.owner_id AND d.blocks_spend) AS paused,
            floor(extract(epoch FROM stamp))::bigint AS observed,floor(extract(epoch FROM q.expires_at))::bigint AS expiry,q.expires_at<=stamp AS expired
            FROM quotes q JOIN budget_roots b USING(owner_id,root_budget_id) JOIN wallets w USING(owner_id)
            CROSS JOIN LATERAL (SELECT w.earned::numeric+w.gift::numeric+w.purchased::numeric AS available) totals
            CROSS JOIN (SELECT clock_timestamp() AS stamp) clock WHERE q.quote_id=$1 AND q.owner_id=$2")
            .bind(quote).bind(owner).fetch_one(&mut *tx).await?;
        let max: i64 = r.try_get("max_amount")?;
        let remaining: i64 = r.try_get("remaining")?;
        let status = if r.try_get("expired")? {
            FundingStatus::Expired
        } else if !r.try_get::<bool, _>("open")? {
            FundingStatus::BudgetClosed
        } else if r.try_get("paused")? {
            FundingStatus::SpendingPaused
        } else if remaining < max {
            FundingStatus::InsufficientBudget
        } else if !r.try_get::<bool, _>("covered")? {
            FundingStatus::InsufficientFunds
        } else {
            FundingStatus::Sufficient
        };
        let wallet_available = match role {
            None | Some(TeamRole::Owner) => Some(r.try_get("wallet")?),
            _ => None,
        };
        let view = QuoteFundingView {
            quote_id: quote.to_owned(),
            requester_id: actor.user.clone(),
            owner_id: owner.to_owned(),
            root_budget_id: root,
            account_kind: if role.is_some() {
                FundingAccount::Team
            } else {
                FundingAccount::Personal
            },
            price_version: r.try_get("price_version")?,
            max_microcredits: max.to_string(),
            budget_limit: r.try_get::<i64, _>("limit_amount")?.to_string(),
            budget_spent: r.try_get::<i64, _>("spent")?.to_string(),
            budget_reserved: r.try_get::<i64, _>("reserved")?.to_string(),
            budget_remaining: remaining.to_string(),
            wallet_available,
            status,
            observed_unix: r.try_get::<i64, _>("observed")?.to_string(),
            expires_unix: r.try_get::<i64, _>("expiry")?.to_string(),
        };
        authority::finish(tx, actor).await?;
        Ok(view)
    }
}
