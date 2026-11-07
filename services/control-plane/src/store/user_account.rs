//! Personal budget creation/reads stay in the original user-session transaction.
use super::*;
use crate::auth::users::UserSessionGuard;
use orange_protocol::account::{PersonalBudgetList, WalletView};
impl Store {
    pub(crate) async fn personal_wallet(
        guard: &mut UserSessionGuard,
    ) -> Result<WalletView, StoreError> {
        let user = guard.view().user_id.clone();
        let owner = guard.view().personal_owner_id.clone();
        let row = sqlx::query("SELECT * FROM wallets WHERE owner_id=$1")
            .bind(&owner)
            .fetch_one(&mut **guard.transaction())
            .await?;
        Ok(WalletView {
            user_id: user,
            owner_id: owner,
            gift: Money(credits(row.try_get("gift")?)?),
            earned: Money(credits(row.try_get("earned")?)?),
            purchased: Money(credits(row.try_get("purchased")?)?),
            reserved_gift: Money(credits(row.try_get("reserved_gift")?)?),
            reserved_earned: Money(credits(row.try_get("reserved_earned")?)?),
            reserved_purchased: Money(credits(row.try_get("reserved_purchased")?)?),
            pending_earned: Money(credits(row.try_get("pending_earned")?)?),
        })
    }
    pub(crate) async fn personal_budget(
        guard: &mut UserSessionGuard,
        root: &str,
    ) -> Result<BudgetView, StoreError> {
        if !valid_id(root) {
            return Err(StoreError::Invalid);
        }
        let owner = guard.view().personal_owner_id.clone();
        let row = sqlx::query("SELECT * FROM budget_roots WHERE owner_id=$1 AND root_budget_id=$2")
            .bind(&owner)
            .bind(root)
            .fetch_optional(&mut **guard.transaction())
            .await?
            .ok_or(StoreError::NotFound)?;
        budget_view(&row)
    }
    pub(crate) async fn personal_budgets(
        guard: &mut UserSessionGuard,
        after: &str,
        limit: u16,
    ) -> Result<PersonalBudgetList, StoreError> {
        if (!after.is_empty() && !valid_id(after)) || !(1..=100).contains(&limit) {
            return Err(StoreError::Invalid);
        }
        let user = guard.view().user_id.clone();
        let owner = guard.view().personal_owner_id.clone();
        let rows=sqlx::query("SELECT * FROM budget_roots WHERE owner_id=$1 AND root_budget_id>$2 ORDER BY root_budget_id LIMIT $3").bind(&owner).bind(after).bind(i64::from(limit)+1).fetch_all(&mut **guard.transaction()).await?;
        let more = rows.len() > usize::from(limit);
        let mut data = Vec::new();
        for row in rows.iter().take(usize::from(limit)) {
            data.push(budget_view(row)?);
        }
        let next_cursor = if more {
            data.last().map(|r| r.root_budget_id.clone())
        } else {
            None
        };
        Ok(PersonalBudgetList {
            user_id: user,
            owner_id: owner,
            data,
            next_cursor,
        })
    }
    pub(crate) async fn create_personal_budget(
        guard: &mut UserSessionGuard,
        key: &str,
        request: &CreateBudget,
    ) -> Result<BudgetView, StoreError> {
        if !valid_id(key) || !valid_id(&request.scope_id) {
            return Err(StoreError::Invalid);
        }
        let owner = guard.view().personal_owner_id.clone();
        let tx = guard.transaction();
        let digest = request_digest("user.create_personal_budget.v1", request)
            .map_err(|_| StoreError::Invalid)?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut **tx)
            .await?;
        // Serialize same-owner creation without upgrading the auth guard's owner
        // SHARE lock (two concurrent sessions must not deadlock during an upgrade).
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("orange.personal_budget:{owner}"))
            .execute(&mut **tx)
            .await?;
        if let Some(root) = prior(tx, &owner, "user_create_budget.v1", key, &digest).await? {
            let row =
                sqlx::query("SELECT * FROM budget_roots WHERE owner_id=$1 AND root_budget_id=$2")
                    .bind(&owner)
                    .bind(&root)
                    .fetch_one(&mut **tx)
                    .await?;
            return budget_view(&row);
        }
        let root = Uuid::new_v4().to_string();
        let row=sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES($1,$2,$3,$4) RETURNING *").bind(&root).bind(&owner).bind(&request.scope_id).bind(request.limit.0.units()).fetch_one(&mut **tx).await?;
        record_key(tx, &owner, "user_create_budget.v1", key, &digest, &root).await?;
        emit(tx, &owner, &root, "budget.created", &digest).await?;
        budget_view(&row)
    }
}
