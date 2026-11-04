use orange_domain::{Budget, Credits};
use orange_protocol::{
    BudgetView, CreateBudget, JobView, Money, SubmitJob, request_digest, valid_digest, valid_id,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use uuid::Uuid;

#[derive(Debug)]
pub enum StoreError {
    Invalid,
    Forbidden,
    NotFound,
    Conflict,
    Budget,
    Balance,
    QuoteExpired,
    QuoteMismatch,
    InvalidTransition,
    Database(sqlx::Error),
    Migration(sqlx::migrate::MigrateError),
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(_) => {
                f.write_str("database operation failed; inspect local diagnostics")
            }
            Self::Migration(_) => f.write_str("database migration failed"),
            other => write!(f, "{other:?}"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error()
            .is_some_and(|d| d.code().as_deref() == Some("23505"))
        {
            Self::Conflict
        } else {
            Self::Database(e)
        }
    }
}
impl StoreError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Database(e) if e.as_database_error().is_some_and(|d|
        matches!(d.code().as_deref(), Some("40001" | "40P01"))))
    }
}
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}
impl Store {
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!()
            .run(&self.pool)
            .await
            .map_err(StoreError::Migration)
    }
    async fn begin(&self) -> Result<Transaction<'_, Postgres>, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
    pub async fn budget(&self, owner: &str, root: &str) -> Result<BudgetView, StoreError> {
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner).await?;
        let row = sqlx::query("SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2")
            .bind(root)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let view = budget_view(&row)?;
        tx.commit().await?;
        Ok(view)
    }
    pub async fn create_budget(
        &self,
        owner: &str,
        key: &str,
        req: &CreateBudget,
    ) -> Result<BudgetView, StoreError> {
        if !valid_id(owner) || !valid_id(key) || !valid_id(&req.scope_id) {
            return Err(StoreError::Invalid);
        }
        for attempt in 0..3 {
            match self.create_budget_once(owner, key, req).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => return result,
            }
        }
        unreachable!("bounded loop returns")
    }
    async fn create_budget_once(
        &self,
        owner: &str,
        key: &str,
        req: &CreateBudget,
    ) -> Result<BudgetView, StoreError> {
        let digest = request_digest("create_budget", req).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner).await?;
        if let Some(root) = prior(&mut tx, owner, "create_budget", key, &digest).await? {
            let row =
                sqlx::query("SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2")
                    .bind(root)
                    .bind(owner)
                    .fetch_one(&mut *tx)
                    .await?;
            let view = budget_view(&row)?;
            tx.commit().await?;
            return Ok(view);
        }
        let root = Uuid::new_v4().to_string();
        let row = sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES($1,$2,$3,$4) RETURNING *")
            .bind(&root).bind(owner).bind(&req.scope_id).bind(req.limit.0.units()).fetch_one(&mut *tx).await?;
        record_key(&mut tx, owner, "create_budget", key, &digest, &root).await?;
        emit(&mut tx, owner, &root, "budget.created", &digest).await?;
        let view = budget_view(&row)?;
        tx.commit().await?;
        Ok(view)
    }
    pub async fn submit(
        &self,
        owner: &str,
        key: &str,
        req: &SubmitJob,
    ) -> Result<JobView, StoreError> {
        if ![
            owner,
            key,
            &req.root_budget_id,
            &req.invocation_id,
            &req.quote_id,
        ]
        .iter()
        .all(|s| valid_id(s))
            || !valid_digest(&req.request_projection_digest)
        {
            return Err(StoreError::Invalid);
        }
        for attempt in 0..3 {
            match self.submit_once(owner, key, req).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => return result,
            }
        }
        unreachable!("bounded loop returns")
    }
    async fn submit_once(
        &self,
        owner: &str,
        key: &str,
        req: &SubmitJob,
    ) -> Result<JobView, StoreError> {
        let digest = request_digest("submit_job", req).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner).await?;
        // Lock root before wallet on every path. PostgreSQL, not client memory, arbitrates.
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(&req.root_budget_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        if let Some(job) = prior(&mut tx, owner, "submit_job", key, &digest).await? {
            let view = job_in_tx(&mut tx, owner, &job).await?;
            tx.commit().await?;
            return Ok(view);
        }
        if let Some(row) = sqlx::query("SELECT * FROM jobs WHERE owner_id=$1 AND invocation_id=$2")
            .bind(owner)
            .bind(&req.invocation_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.try_get::<String, _>("request_digest")? != digest {
                return Err(StoreError::Conflict);
            }
            let view = job_view(&row)?;
            record_key(&mut tx, owner, "submit_job", key, &digest, &view.job_id).await?;
            tx.commit().await?;
            return Ok(view);
        }
        let quote = sqlx::query("SELECT *, expires_at > clock_timestamp() AS valid FROM quotes WHERE quote_id=$1 AND owner_id=$2")
            .bind(&req.quote_id).bind(owner).fetch_optional(&mut *tx).await?.ok_or(StoreError::NotFound)?;
        if !quote.try_get::<bool, _>("valid")? {
            return Err(StoreError::QuoteExpired);
        }
        if quote.try_get::<String, _>("root_budget_id")? != req.root_budget_id
            || quote.try_get::<String, _>("invocation_id")? != req.invocation_id
            || quote.try_get::<String, _>("projection_digest")? != req.request_projection_digest
        {
            return Err(StoreError::QuoteMismatch);
        }
        let maximum = credits(quote.try_get("max_amount")?)?;
        let next = budget_model(&root)?
            .reserve(maximum)
            .map_err(|_| StoreError::Budget)?;
        let wallet = sqlx::query("SELECT * FROM wallets WHERE owner_id=$1 FOR UPDATE")
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Balance)?;
        let gift: i64 = wallet.try_get("gift")?;
        let earned: i64 = wallet.try_get("earned")?;
        let purchased: i64 = wallet.try_get("purchased")?;
        let [hg, he, hp] = allocate(maximum.units(), [gift, earned, purchased])?;
        sqlx::query("UPDATE wallets SET gift=gift-$2,earned=earned-$3,purchased=purchased-$4,reserved_gift=reserved_gift+$2,reserved_earned=reserved_earned+$3,reserved_purchased=reserved_purchased+$4 WHERE owner_id=$1")
            .bind(owner).bind(hg).bind(he).bind(hp).execute(&mut *tx).await?;
        sqlx::query("UPDATE budget_roots SET reserved=$2 WHERE root_budget_id=$1")
            .bind(&req.root_budget_id)
            .bind(next.reserved.units())
            .execute(&mut *tx)
            .await?;
        let job = Uuid::new_v4().to_string();
        let purpose = serde_json::to_value(&req.purpose).map_err(|_| StoreError::Invalid)?;
        let row = sqlx::query("INSERT INTO jobs(job_id,owner_id,root_budget_id,invocation_id,purpose,quote_id,request_digest,execution_state,reserved,held_gift,held_earned,held_purchased) VALUES($1,$2,$3,$4,$5,$6,$7,'queued',$8,$9,$10,$11) RETURNING *")
            .bind(&job).bind(owner).bind(&req.root_budget_id).bind(&req.invocation_id).bind(purpose.as_str().ok_or(StoreError::Invalid)?)
            .bind(&req.quote_id).bind(&digest).bind(maximum.units()).bind(hg).bind(he).bind(hp).fetch_one(&mut *tx).await?;
        let mut entries = Vec::new();
        for (bucket, held) in ["gift", "earned", "purchased"].iter().zip([hg, he, hp]) {
            entries.push((format!("{owner}:{bucket}:available"), -held));
            entries.push((format!("{owner}:{bucket}:reserved"), held));
        }
        ledger(&mut tx, owner, &format!("reserve:{job}"), entries).await?;
        record_key(&mut tx, owner, "submit_job", key, &digest, &job).await?;
        emit(&mut tx, owner, &job, "job.queued", &digest).await?;
        let view = job_view(&row)?;
        tx.commit().await?;
        Ok(view)
    }
    pub async fn job(&self, owner: &str, id: &str) -> Result<JobView, StoreError> {
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner).await?;
        let view = job_in_tx(&mut tx, owner, id).await?;
        tx.commit().await?;
        Ok(view)
    }
    /// Queue-only cancellation is a real release. Running work requires the future execution lifecycle.
    pub async fn cancel_queued(
        &self,
        owner: &str,
        id: &str,
        key: &str,
    ) -> Result<JobView, StoreError> {
        if ![owner, id, key].iter().all(|s| valid_id(s)) {
            return Err(StoreError::Invalid);
        }
        for attempt in 0..3 {
            match self.cancel_once(owner, id, key).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => return result,
            }
        }
        unreachable!("bounded loop returns")
    }
    async fn cancel_once(&self, owner: &str, id: &str, key: &str) -> Result<JobView, StoreError> {
        let digest = request_digest("cancel_job", &id).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner).await?;
        let lookup = sqlx::query("SELECT root_budget_id FROM jobs WHERE job_id=$1 AND owner_id=$2")
            .bind(id)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let root_id: String = lookup.try_get("root_budget_id")?;
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(&root_id)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT * FROM jobs WHERE job_id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(id)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
        let current = job_view(&row)?;
        if prior(&mut tx, owner, "cancel_job", key, &digest)
            .await?
            .is_some()
        {
            tx.commit().await?;
            return Ok(current);
        }
        if current.execution_state == "cancelled" && current.settlement_state == "settled" {
            record_key(&mut tx, owner, "cancel_job", key, &digest, id).await?;
            tx.commit().await?;
            return Ok(current);
        }
        if current.execution_state != "queued" {
            return Err(StoreError::InvalidTransition);
        }
        let next = budget_model(&root)?
            .settle(current.reserved.0, Credits::ZERO)
            .map_err(|_| StoreError::Budget)?;
        let held: [i64; 3] = [
            row.try_get("held_gift")?,
            row.try_get("held_earned")?,
            row.try_get("held_purchased")?,
        ];
        sqlx::query("UPDATE wallets SET gift=gift+$2,earned=earned+$3,purchased=purchased+$4,reserved_gift=reserved_gift-$2,reserved_earned=reserved_earned-$3,reserved_purchased=reserved_purchased-$4 WHERE owner_id=$1")
            .bind(owner).bind(held[0]).bind(held[1]).bind(held[2]).execute(&mut *tx).await?;
        sqlx::query("UPDATE budget_roots SET reserved=$2 WHERE root_budget_id=$1")
            .bind(&root_id)
            .bind(next.reserved.units())
            .execute(&mut *tx)
            .await?;
        let mut entries = Vec::new();
        for (bucket, amount) in ["gift", "earned", "purchased"].iter().zip(held) {
            entries.push((format!("{owner}:{bucket}:reserved"), -amount));
            entries.push((format!("{owner}:{bucket}:available"), amount));
        }
        ledger(&mut tx, owner, &format!("release:{id}"), entries).await?;
        record_key(&mut tx, owner, "cancel_job", key, &digest, id).await?;
        sqlx::query("UPDATE jobs SET execution_state='cancelled',settlement_state='settled',charge=0 WHERE job_id=$1")
            .bind(id).execute(&mut *tx).await?;
        emit(
            &mut tx,
            owner,
            id,
            "job.cancelled",
            "zero_charge_queue_cancel",
        )
        .await?;
        let view = job_in_tx(&mut tx, owner, id).await?;
        tx.commit().await?;
        Ok(view)
    }
}

async fn retry_backoff(attempt: u32) {
    // Retry the whole transaction, never a payment/tool side effect. Bounded jitter
    // prevents all contenders from taking a new SERIALIZABLE snapshot together.
    let jitter = (Uuid::new_v4().as_u128() % 17) as u64;
    tokio::time::sleep(std::time::Duration::from_millis(
        (20_u64 << attempt) + jitter,
    ))
    .await;
}

fn allocate(amount: i64, balances: [i64; 3]) -> Result<[i64; 3], StoreError> {
    let mut remaining = amount;
    let mut held = [0; 3];
    for i in 0..3 {
        held[i] = remaining.min(balances[i]);
        remaining -= held[i];
    }
    if remaining != 0 {
        Err(StoreError::Balance)
    } else {
        Ok(held)
    }
}
fn credits(n: i64) -> Result<Credits, StoreError> {
    Credits::new(n).map_err(|_| StoreError::Invalid)
}
fn budget_model(r: &sqlx::postgres::PgRow) -> Result<Budget, StoreError> {
    Ok(Budget {
        limit: credits(r.try_get("limit_amount")?)?,
        spent: credits(r.try_get("spent")?)?,
        reserved: credits(r.try_get("reserved")?)?,
        closing: r.try_get::<String, _>("state")? != "active",
    })
}
fn budget_view(r: &sqlx::postgres::PgRow) -> Result<BudgetView, StoreError> {
    let b = budget_model(r)?;
    Ok(BudgetView {
        root_budget_id: r.try_get("root_budget_id")?,
        scope_id: r.try_get("scope_id")?,
        limit: Money(b.limit),
        spent: Money(b.spent),
        reserved: Money(b.reserved),
        status: r.try_get("state")?,
    })
}
fn job_view(r: &sqlx::postgres::PgRow) -> Result<JobView, StoreError> {
    let charge: Option<i64> = r.try_get("charge")?;
    Ok(JobView {
        job_id: r.try_get("job_id")?,
        root_budget_id: r.try_get("root_budget_id")?,
        invocation_id: r.try_get("invocation_id")?,
        execution_state: r.try_get("execution_state")?,
        settlement_state: r.try_get("settlement_state")?,
        reserved: Money(credits(r.try_get("reserved")?)?),
        charge: charge.map(credits).transpose()?.map(Money),
    })
}
async fn authorize(tx: &mut Transaction<'_, Postgres>, owner: &str) -> Result<(), StoreError> {
    if sqlx::query("SELECT owner_id FROM owners WHERE owner_id=$1 AND state='active' FOR SHARE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .is_none()
    {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}
async fn job_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    id: &str,
) -> Result<JobView, StoreError> {
    let row = sqlx::query("SELECT * FROM jobs WHERE job_id=$1 AND owner_id=$2")
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound)?;
    job_view(&row)
}
async fn prior(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    operation: &str,
    key: &str,
    digest: &str,
) -> Result<Option<String>, StoreError> {
    if let Some(row) = sqlx::query(
        "SELECT digest,resource_id FROM idempotency WHERE owner_id=$1 AND operation=$2 AND key=$3",
    )
    .bind(owner)
    .bind(operation)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await?
    {
        if row.try_get::<String, _>("digest")? != digest {
            return Err(StoreError::Conflict);
        }
        return Ok(Some(row.try_get("resource_id")?));
    }
    Ok(None)
}
async fn record_key(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    operation: &str,
    key: &str,
    digest: &str,
    id: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO idempotency(owner_id,operation,key,digest,resource_id) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(owner)
    .bind(operation)
    .bind(key)
    .bind(digest)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
async fn emit(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    id: &str,
    kind: &str,
    payload: &str,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO outbox(owner_id,entity_id,event_type,payload) VALUES($1,$2,$3,$4)")
        .bind(owner)
        .bind(id)
        .bind(kind)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn ledger(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    key: &str,
    entries: Vec<(String, i64)>,
) -> Result<(), StoreError> {
    if entries.iter().map(|(_, v)| i128::from(*v)).sum::<i128>() != 0 {
        return Err(StoreError::Invalid);
    }
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(&id)
    .bind(key)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    for (account, amount) in entries {
        sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,$2,$3)")
            .bind(&id)
            .bind(account)
            .bind(amount)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
