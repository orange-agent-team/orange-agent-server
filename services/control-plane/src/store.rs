mod queue_expiry;
mod user_account;
mod withdrawal;
use orange_domain::{Budget, Credits};
use orange_protocol::{
    BudgetView, CreateBudget, JobView, Money, SubmitJob, request_digest, valid_digest, valid_id,
};
pub use queue_expiry::QueueExpiryCursor;
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
    SpendingPaused,
    InvalidTransition,
    StaleJobVersion,
    InvocationWithdrawn,
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
        match e
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref()
        {
            Some("23505") => Self::Conflict,
            Some("OA031") => Self::SpendingPaused,
            _ => Self::Database(e),
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
        self.submit_as(None, owner, key, req, None).await
    }
    pub async fn submit_for_user(
        &self,
        actor: &crate::artifacts::ArtifactActor,
        owner: &str,
        key: &str,
        req: &SubmitJob,
    ) -> Result<JobView, StoreError> {
        if !matches!(
            req.purpose,
            orange_protocol::InvocationPurpose::Primary
                | orange_protocol::InvocationPurpose::Summary
                | orange_protocol::InvocationPurpose::Memory
                | orange_protocol::InvocationPurpose::Title
                | orange_protocol::InvocationPurpose::Continuation
                | orange_protocol::InvocationPurpose::Transcription
        ) {
            return Err(StoreError::Invalid);
        }
        self.submit_as(Some(actor), owner, key, req, None).await
    }
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }
    pub(crate) async fn submit_cloud(
        &self,
        actor: &crate::artifacts::ArtifactActor,
        owner: &str,
        key: &str,
        req: &SubmitJob,
        run: &str,
    ) -> Result<JobView, StoreError> {
        self.submit_as(Some(actor), owner, key, req, Some(run))
            .await
    }
    async fn submit_as(
        &self,
        actor: Option<&crate::artifacts::ArtifactActor>,
        owner: &str,
        key: &str,
        req: &SubmitJob,
        origin: Option<&str>,
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
            match self.submit_once(owner, key, req, actor, origin).await {
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
        actor: Option<&crate::artifacts::ArtifactActor>,
        origin: Option<&str>,
    ) -> Result<JobView, StoreError> {
        let digest = request_digest("submit_job", req).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        if let Some(actor) = actor {
            crate::artifacts::authority::scope(&mut tx, actor, owner, true)
                .await
                .map_err(artifact_error)?;
        }
        authorize(&mut tx, owner).await?;
        withdrawal::submission_lock(&mut tx, owner, &req.invocation_id).await?;
        crate::cloud_model::authorize_submit(
            &mut tx,
            owner,
            actor.map(|a| a.user.as_str()),
            req,
            origin,
        )
        .await?;
        if let Some(actor) = actor {
            check_budget_actor(&mut tx, &actor.user, owner, &req.root_budget_id).await?;
        }
        let asr_requester: Option<String> = sqlx::query_scalar(
            "SELECT requester_id FROM transcription_quote_terms WHERE quote_id=$1 AND owner_id=$2",
        )
        .bind(&req.quote_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
        if asr_requester.as_deref().is_some_and(|u| {
            actor.map(|a| a.user.as_str()) != Some(u)
                || req.purpose != orange_protocol::InvocationPurpose::Transcription
        }) || (actor.is_some()
            && req.purpose == orange_protocol::InvocationPurpose::Transcription
            && asr_requester.is_none())
        {
            return Err(StoreError::Forbidden);
        }
        let priced_requester: Option<String> = sqlx::query_scalar(
            "SELECT requester_id FROM quote_metering WHERE quote_id=$1 AND owner_id=$2",
        )
        .bind(&req.quote_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
        if priced_requester
            .as_deref()
            .is_some_and(|user| actor.map(|a| a.user.as_str()) != Some(user))
        {
            return Err(StoreError::Forbidden);
        }
        if let Some(job) = prior(&mut tx, owner, "submit_job", key, &digest).await? {
            check_submit_actor(&mut tx, &job, actor).await?;
            let view = job_in_tx(&mut tx, owner, &job).await?;
            finish_submit(tx, actor).await?;
            return Ok(view);
        }
        if let Some(row) = sqlx::query("SELECT job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge,request_digest FROM jobs WHERE owner_id=$1 AND invocation_id=$2")
            .bind(owner)
            .bind(&req.invocation_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.try_get::<String, _>("request_digest")? != digest {
                return Err(StoreError::Conflict);
            }
            check_submit_actor(&mut tx, &row.try_get::<String, _>("job_id")?, actor).await?;
            let view = job_view(&row)?;
            record_key(&mut tx, owner, "submit_job", key, &digest, &view.job_id).await?;
            finish_submit(tx, actor).await?;
            return Ok(view);
        }
        withdrawal::check_not_withdrawn(&mut tx, owner, actor, &req.invocation_id).await?;
        if let Some(user) = asr_requester {
            crate::artifacts::transcription_pricing::admitted(&mut tx, &req.quote_id, owner, &user)
                .await
                .map_err(artifact_error)?;
        }
        // A quote fixes terms, but does not preserve content access. Hold the exact
        // input version through the new reservation so user deletion and submission
        // have one observable order even before the separate input binding arrives.
        if priced_requester.is_some() {
            let source=sqlx::query("SELECT a.* FROM quote_metering m JOIN artifacts a
                ON a.artifact_id=m.input_artifact_id AND a.version_id=m.input_version_id AND a.owner_id=m.owner_id
                WHERE m.quote_id=$1 AND m.owner_id=$2 AND a.state='available' FOR SHARE OF a")
                .bind(&req.quote_id).bind(owner).fetch_optional(&mut *tx).await?.ok_or(StoreError::Forbidden)?;
            crate::artifacts::authority::resource(
                &mut tx,
                actor.ok_or(StoreError::Forbidden)?,
                owner,
                &source,
                false,
                false,
            )
            .await
            .map_err(artifact_error)?;
        }
        let invalid_price:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM quote_metering m JOIN text_prices p USING(price_version) JOIN trusted_meter_profiles mp ON mp.profile_digest=m.meter_profile_digest WHERE m.quote_id=$1 AND (NOT p.enabled OR NOT mp.enabled))").bind(&req.quote_id).fetch_one(&mut *tx).await?;
        if invalid_price {
            return Err(StoreError::QuoteMismatch);
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
        // Lock root before wallet on every path. PostgreSQL, not client memory, arbitrates.
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(&req.root_budget_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
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
        let row = sqlx::query("INSERT INTO jobs(job_id,owner_id,root_budget_id,invocation_id,purpose,quote_id,request_digest,execution_state,reserved,held_gift,held_earned,held_purchased,submitting_user_id,input_required) VALUES($1,$2,$3,$4,$5,$6,$7,'queued',$8,$9,$10,$11,$12,$13) RETURNING job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge")
            .bind(&job).bind(owner).bind(&req.root_budget_id).bind(&req.invocation_id).bind(purpose.as_str().ok_or(StoreError::Invalid)?)
            .bind(&req.quote_id).bind(&digest).bind(maximum.units()).bind(hg).bind(he).bind(hp).bind(actor.map(|a|a.user.as_str())).bind(actor.is_some()).fetch_one(&mut *tx).await?;
        let mut entries = Vec::new();
        for (bucket, held) in ["gift", "earned", "purchased"].iter().zip([hg, he, hp]) {
            entries.push((format!("{owner}:{bucket}:available"), -held));
            entries.push((format!("{owner}:{bucket}:reserved"), held));
        }
        ledger(&mut tx, owner, &format!("reserve:{job}"), entries).await?;
        record_key(&mut tx, owner, "submit_job", key, &digest, &job).await?;
        emit(&mut tx, owner, &job, "job.queued", &digest).await?;
        if let Some(run) = origin {
            crate::cloud_model::bind_submitted_input(&mut tx, run, &job).await?;
            sqlx::query("INSERT INTO cloud_model_jobs(job_id,run_id,quote_id) VALUES($1,$2,$3)")
                .bind(&job)
                .bind(run)
                .bind(&req.quote_id)
                .execute(&mut *tx)
                .await?;
            let version:i64=sqlx::query_scalar("UPDATE cloud_runs SET status='running',version=version+1 WHERE run_id=$1 AND status IN ('queued','waiting_approval') RETURNING version")
                .bind(run).fetch_optional(&mut *tx).await?.ok_or(StoreError::Conflict)?;
            sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,'model_job_submitted')").bind(run).bind(version).execute(&mut *tx).await?;
            crate::cloud_model::live(
                &mut tx,
                run,
                owner,
                actor.ok_or(StoreError::Forbidden)?.user.as_str(),
            )
            .await
            .map_err(artifact_error)?;
        }

        // A funding lock or a ledger write may have waited past the expiry read at admission.
        // Replays above only return an existing effect, so they retain the original receipt.
        let still_valid: bool = sqlx::query_scalar(
            "SELECT expires_at > clock_timestamp() FROM quotes WHERE quote_id=$1 AND owner_id=$2",
        )
        .bind(&req.quote_id)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
        if !still_valid {
            return Err(StoreError::QuoteExpired);
        }
        let view = job_view(&row)?;
        finish_submit(tx, actor).await?;
        Ok(view)
    }
    pub async fn job(&self, owner: &str, id: &str) -> Result<JobView, StoreError> {
        Ok(self.job_with_revision(owner, id).await?.view)
    }
    pub(crate) async fn job_with_revision(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<crate::job_revision::Status, StoreError> {
        let owner =
            orange_domain::identity::OwnerId::try_from(owner).map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        authorize(&mut tx, owner.as_str()).await?;
        let job = orange_domain::identity::JobId::try_from(id).map_err(|_| StoreError::Invalid)?;
        let row = sqlx::query("SELECT job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge,state_revision FROM jobs WHERE owner_id=$1 AND job_id=$2")
            .bind(owner.as_str())
            .bind(job.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let status = crate::job_revision::Status {
            view: job_view(&row)?,
            revision: row.try_get("state_revision")?,
        };
        tx.commit().await?;
        Ok(status)
    }
    /// Development bearer entry remains queue-only. Current user sessions use
    /// cancel_for_user, which requests running cancellation without releasing funds.
    pub async fn cancel_queued(
        &self,
        owner: &str,
        id: &str,
        key: &str,
    ) -> Result<JobView, StoreError> {
        self.cancel_as(None, owner, id, key, None).await
    }
    pub async fn cancel_for_user(
        &self,
        actor: &crate::artifacts::ArtifactActor,
        owner: &str,
        id: &str,
        key: &str,
    ) -> Result<JobView, StoreError> {
        if !actor.write {
            return Err(StoreError::Forbidden);
        }
        self.cancel_as(Some(actor), owner, id, key, None).await
    }
    pub(crate) async fn cancel_as(
        &self,
        actor: Option<&crate::artifacts::ArtifactActor>,
        owner: &str,
        id: &str,
        key: &str,
        expected: Option<crate::job_revision::Expected>,
    ) -> Result<JobView, StoreError> {
        if actor.is_some_and(|actor| !actor.write) {
            return Err(StoreError::Forbidden);
        }
        if ![owner, id, key].iter().all(|s| valid_id(s)) {
            return Err(StoreError::Invalid);
        }
        for attempt in 0..3 {
            match self.cancel_once(owner, id, key, actor, expected).await {
                Err(e) if e.retryable() && attempt < 2 => retry_backoff(attempt).await,
                result => return result,
            }
        }
        unreachable!("bounded loop returns")
    }
    async fn cancel_once(
        &self,
        owner: &str,
        id: &str,
        key: &str,
        actor: Option<&crate::artifacts::ArtifactActor>,
        expected: Option<crate::job_revision::Expected>,
    ) -> Result<JobView, StoreError> {
        let (operation, digest) = if let Some(actor) = actor {
            (
                format!("user_cancel_job:{}", actor.user),
                request_digest("user_cancel_job.v1", &(id, &actor.user)),
            )
        } else {
            ("cancel_job".into(), request_digest("cancel_job", &id))
        };
        let digest = if let Some(check) = expected {
            request_digest("job.cancel.precondition.v1", &(&operation, id, check.get()))
        } else {
            digest
        };
        let digest = digest.map_err(|_| StoreError::Invalid)?;
        let mut tx = self.begin().await?;
        sqlx::query("SET LOCAL lock_timeout='3s'")
            .execute(&mut *tx)
            .await?;
        if let Some(actor) = actor {
            // Stop one's accepted work even after source ACL/spending revocation
            // or downgrade to viewer. No new effect or content read is granted.
            crate::artifacts::authority::scope(&mut tx, actor, owner, false)
                .await
                .map_err(artifact_error)?;
        }
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
        let row = sqlx::query("SELECT job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge,owner_id,state_revision,held_gift,held_earned,held_purchased FROM jobs WHERE job_id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(id)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
        if actor.is_some() {
            check_submit_actor(&mut tx, id, actor).await?;
        }
        let current = job_view(&row)?;
        if prior(&mut tx, owner, &operation, key, &digest)
            .await?
            .is_some()
        {
            finish_submit(tx, actor).await?;
            return Ok(current);
        }
        let revision: i64 = row.try_get("state_revision")?;
        if expected.is_some_and(|value| value.get() != revision) {
            return Err(StoreError::StaleJobVersion);
        }
        if current.execution_state == "cancelled" && current.settlement_state == "settled" {
            record_key(&mut tx, owner, &operation, key, &digest, id).await?;
            finish_submit(tx, actor).await?;
            return Ok(current);
        }
        if actor.is_some()
            && matches!(
                current.execution_state.as_str(),
                "succeeded" | "failed" | "cancelled"
            )
        {
            // Delivery/cleanup/settlement remain the original terminal facts.
            record_key(&mut tx, owner, &operation, key, &digest, id).await?;
            finish_submit(tx, actor).await?;
            return Ok(current);
        }
        if actor.is_some() && matches!(current.execution_state.as_str(), "running" | "cancelling") {
            if current.settlement_state != "pending" {
                return Err(StoreError::InvalidTransition);
            }
            record_key(&mut tx, owner, &operation, key, &digest, id).await?;
            if current.execution_state == "running" {
                sqlx::query("UPDATE jobs SET execution_state='cancelling' WHERE job_id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                emit(&mut tx, owner, id, "job.cancel_requested", &digest).await?;
            }
            let view = job_in_tx(&mut tx, owner, id).await?;
            finish_submit(tx, actor).await?;
            return Ok(view);
        }
        if current.execution_state != "queued" || current.settlement_state != "pending" {
            return Err(StoreError::InvalidTransition);
        }
        queue_expiry::release_queue(&mut tx, &root, &row, queue_expiry::QueueEnd::Cancelled)
            .await?;
        record_key(&mut tx, owner, &operation, key, &digest, id).await?;
        let view = job_in_tx(&mut tx, owner, id).await?;
        finish_submit(tx, actor).await?;
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

pub(crate) fn allocate(amount: i64, balances: [i64; 3]) -> Result<[i64; 3], StoreError> {
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
pub(crate) fn budget_model(r: &sqlx::postgres::PgRow) -> Result<Budget, StoreError> {
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
pub(crate) fn job_view(r: &sqlx::postgres::PgRow) -> Result<JobView, StoreError> {
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
    let row = sqlx::query("SELECT job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge FROM jobs WHERE job_id=$1 AND owner_id=$2")
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
    crate::events::append(tx, owner, id, kind, payload).await?;
    Ok(())
}
pub(crate) async fn ledger(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    key: &str,
    entries: Vec<(String, i64)>,
) -> Result<(), StoreError> {
    if entries.iter().map(|(_, v)| i128::from(*v)).sum::<i128>() != 0 {
        return Err(StoreError::Invalid);
    }
    use orange_domain::identity::{LedgerTransactionId, OwnerId};
    let owner = OwnerId::try_from(owner).map_err(|_| StoreError::Invalid)?;
    let key_id = Uuid::new_v4().to_string();
    let id = LedgerTransactionId::try_from(key_id.as_str()).map_err(|_| StoreError::Invalid)?;
    sqlx::query(
        "INSERT INTO ledger_transactions(transaction_id,business_key,owner_id) VALUES($1,$2,$3)",
    )
    .bind(id.as_str())
    .bind(key)
    .bind(owner.as_str())
    .execute(&mut **tx)
    .await?;
    for (account, amount) in entries {
        sqlx::query("INSERT INTO ledger_entries(transaction_id,account,amount) VALUES($1,$2,$3)")
            .bind(id.as_str())
            .bind(account)
            .bind(amount)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub(crate) fn artifact_error(e: crate::artifacts::ArtifactError) -> StoreError {
    match e {
        crate::artifacts::ArtifactError::Database(e) => StoreError::Database(e),
        _ => StoreError::Forbidden,
    }
}
async fn finish_submit(
    tx: Transaction<'_, Postgres>,
    actor: Option<&crate::artifacts::ArtifactActor>,
) -> Result<(), StoreError> {
    if let Some(actor) = actor {
        crate::artifacts::authority::finish(tx, actor)
            .await
            .map_err(artifact_error)
    } else {
        tx.commit().await?;
        Ok(())
    }
}
async fn check_submit_actor(
    tx: &mut Transaction<'_, Postgres>,
    job: &str,
    actor: Option<&crate::artifacts::ArtifactActor>,
) -> Result<(), StoreError> {
    let submitted: Option<String> =
        sqlx::query_scalar("SELECT submitting_user_id FROM jobs WHERE job_id=$1")
            .bind(job)
            .fetch_one(&mut **tx)
            .await?;
    if submitted.as_deref() != actor.map(|a| a.user.as_str()) {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}

pub(crate) async fn check_budget_actor(
    tx: &mut Transaction<'_, Postgres>,
    user: &str,
    owner: &str,
    root: &str,
) -> Result<(), StoreError> {
    let personal: Option<String> = sqlx::query_scalar(
        "SELECT personal_owner_id FROM users WHERE user_id=$1 AND active FOR SHARE",
    )
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    if personal.as_deref() == Some(owner) {
        return Ok(());
    }
    let granted:Option<bool>=sqlx::query_scalar("SELECT can_submit FROM budget_user_grants WHERE root_budget_id=$1 AND user_id=$2 FOR SHARE").bind(root).bind(user).fetch_optional(&mut **tx).await?;
    if granted != Some(true) {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}

pub(crate) async fn lock_model_invocation(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    invocation: &str,
) -> Result<(), StoreError> {
    withdrawal::submission_lock(tx, owner, invocation).await
}

/// Called with the parent Run locked. It may stop only the exact Job that
/// was linked to the Run inside the original wallet reservation transaction.
pub(crate) async fn stop_cloud_job(
    tx: &mut Transaction<'_, Postgres>,
    run: &str,
) -> Result<(), StoreError> {
    let links=sqlx::query("SELECT j.job_id,j.root_budget_id,j.owner_id FROM cloud_model_jobs m JOIN jobs j USING(job_id)
        JOIN cloud_runs r ON r.run_id=m.run_id AND r.root_budget_id=j.root_budget_id AND r.owner_id=j.owner_id
        WHERE m.run_id=$1 AND j.execution_state IN ('queued','running','cancelling') ORDER BY j.job_id").bind(run).fetch_all(&mut **tx).await?;
    for link in links {
        let owner: String = link.try_get("owner_id")?;
        let job: String = link.try_get("job_id")?;
        let root = sqlx::query(
            "SELECT * FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 FOR UPDATE",
        )
        .bind(link.try_get::<String, _>("root_budget_id")?)
        .bind(&owner)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query("SELECT owner_id FROM wallets WHERE owner_id=$1 FOR UPDATE")
            .bind(&owner)
            .fetch_one(&mut **tx)
            .await?;
        let row = sqlx::query("SELECT job_id,root_budget_id,invocation_id,execution_state,settlement_state,reserved,charge,owner_id,held_gift,held_earned,held_purchased FROM jobs WHERE job_id=$1 AND owner_id=$2 FOR UPDATE")
            .bind(&job)
            .bind(&owner)
            .fetch_one(&mut **tx)
            .await?;
        let state: String = row.try_get("execution_state")?;
        match state.as_str() {
            "queued" => {
                queue_expiry::release_queue(tx, &root, &row, queue_expiry::QueueEnd::Cancelled)
                    .await?;
            }
            "running" => {
                if row.try_get::<String, _>("settlement_state")? != "pending" {
                    return Err(StoreError::InvalidTransition);
                }
                let digest = request_digest("cloud.run.stop-model.v1", &(run, &job))
                    .map_err(|_| StoreError::Invalid)?;
                let changed=sqlx::query("UPDATE jobs SET execution_state='cancelling' WHERE job_id=$1 AND execution_state='running' AND settlement_state='pending'")
                .bind(&job).execute(&mut **tx).await?;
                if changed.rows_affected() != 1 {
                    return Err(StoreError::Conflict);
                }
                emit(tx, &owner, &job, "job.cancel_requested", &digest).await?;
            }
            // Existing terminal, cancelling or uncertain facts retain their original
            // accounting, retention and device-cleanup responsibilities.
            _ => {}
        }
    }
    Ok(())
}
