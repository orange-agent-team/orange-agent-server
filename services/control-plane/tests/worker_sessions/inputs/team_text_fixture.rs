//! Transactional development fixtures for the actual browser/Worker path.
use sqlx::PgPool;
use uuid::Uuid;

const TEAM_CREDITS: i64 = 10_000_000;

pub(super) struct Allocation {
    pub owner: String,
    pub budget: String,
    original_personal: (i64, i64, i64, i64, i64, i64),
}

async fn wallet(pool: &PgPool, owner: &str) -> (i64, i64, i64, i64, i64, i64) {
    sqlx::query_as("SELECT gift,earned,purchased,reserved_gift,reserved_earned,reserved_purchased FROM wallets WHERE owner_id=$1")
        .bind(owner).fetch_one(pool).await.expect("original developer-credit buckets")
}

impl Allocation {
    pub async fn seed(pool: &PgPool, user: &str, personal: &str) -> Self {
        let owner = Uuid::new_v4().to_string();
        let creator = Uuid::new_v4().to_string();
        let creator_personal = Uuid::new_v4().to_string();
        let budget = Uuid::new_v4().to_string();
        let mut tx = pool.begin().await.expect("fixture transaction");
        sqlx::query("INSERT INTO owners(owner_id) VALUES($1),($2)")
            .bind(&owner)
            .bind(&creator_personal)
            .execute(&mut *tx)
            .await
            .expect("isolated owners");
        sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'team-text-fixture',$1,$2)").bind(&creator).bind(&creator_personal).execute(&mut *tx).await.expect("team creator");
        sqlx::query(
            "INSERT INTO organizations(organization_id,name) VALUES($1,'Text compute fixture')",
        )
        .bind(&owner)
        .execute(&mut *tx)
        .await
        .expect("organization");
        sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'owner'),($1,$3,'member')").bind(&owner).bind(&creator).bind(user).execute(&mut *tx).await.expect("separate owner and member");
        sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,$2)")
            .bind(&owner)
            .bind(TEAM_CREDITS)
            .execute(&mut *tx)
            .await
            .expect("development gift fixture");
        sqlx::query("UPDATE organization_budget_policies SET owner_ceiling=$2,hard_limit=$2,version=1 WHERE organization_id=$1").bind(&owner).bind(TEAM_CREDITS).execute(&mut *tx).await.expect("team ceiling");
        sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES($1,$2,$3,$4)").bind(&budget).bind(&owner).bind(&budget).bind(TEAM_CREDITS).execute(&mut *tx).await.expect("allocated root");
        sqlx::query("INSERT INTO member_budget_allocations(root_budget_id,organization_id,member_id,creator_id,request_digest) VALUES($1,$2,$3,$4,$5)").bind(&budget).bind(&owner).bind(user).bind(&creator).bind("0".repeat(64)).execute(&mut *tx).await.expect("fixture allocation");
        sqlx::query(
            "INSERT INTO budget_user_grants(root_budget_id,user_id,can_submit) VALUES($1,$2,true)",
        )
        .bind(&budget)
        .bind(user)
        .execute(&mut *tx)
        .await
        .expect("member submit permission");
        tx.commit().await.expect("commit allocation fixture");
        Self {
            owner,
            budget,
            original_personal: wallet(pool, personal).await,
        }
    }
    pub async fn verify(
        &self,
        pool: &PgPool,
        personal: &str,
        job: &str,
        charge: i64,
    ) -> serde_json::Value {
        let actual:(String,String,i64,i64)=sqlx::query_as("SELECT j.owner_id,j.root_budget_id,b.spent,b.reserved FROM jobs j JOIN budget_roots b USING(root_budget_id) WHERE job_id=$1").bind(job).fetch_one(pool).await.expect("actual team budget");
        assert_eq!(actual, (self.owner.clone(), self.budget.clone(), charge, 0));
        assert_eq!(
            wallet(pool, &self.owner).await,
            (TEAM_CREDITS - charge, 0, 0, 0, 0, 0)
        );
        assert_eq!(wallet(pool, personal).await, self.original_personal);
        serde_json::json!({
            "organization":self.owner, "root_budget":self.budget,"job":job,
            "charged_microcredits":charge.to_string(), "remaining_team_hold":"0",
            "member_personal_wallet_unchanged":true, "original_team_gift":TEAM_CREDITS.to_string(),
            "remaining_team_gift":(TEAM_CREDITS-charge).to_string()
        })
    }
}
