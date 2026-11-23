//! Browser sessions manage credentials; only the automation routes accept them.
use super::*;
use crate::store::Store;
use orange_protocol::{
    account::{PersonalBudgetList, WalletView},
    personal_keys::{
        AUDIENCE, KeyStatus, PersonalKeyPage, PersonalKeyView, ReadScope, RegisterPersonalKey,
    },
};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

fn key_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == value)
}

fn write_intent(
    headers: &HeaderMap,
    identity: &UserIdentity,
    csrf: &str,
    id: &str,
) -> Result<(), UserApiError> {
    check_write(headers, &identity.config.user_origin, csrf)?;
    if !key_id(id)
        || headers.get_all("idempotency-key").iter().count() != 1
        || headers.get("idempotency-key").and_then(|v| v.to_str().ok()) != Some(id)
    {
        return Err(AuthError::Invalid.into());
    }
    Ok(())
}

fn view(row: &PgRow) -> Result<PersonalKeyView, UserApiError> {
    let scope = match row.try_get::<&str, _>("scope")? {
        "wallet:read" => ReadScope::Wallet,
        "budgets:read" => ReadScope::Budgets,
        _ => return Err(AuthError::Unavailable.into()),
    };
    Ok(PersonalKeyView {
        key_id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        owner_id: row.try_get("owner_id")?,
        label: row.try_get("label")?,
        scope,
        audience: AUDIENCE.into(),
        status: if row.try_get::<Option<i64>, _>("revoked_unix")?.is_some() {
            KeyStatus::Revoked
        } else if row.try_get("expired")? {
            KeyStatus::Expired
        } else {
            KeyStatus::Active
        },
        created_at: timestamp(row.try_get("created_unix")?)?,
        expires_at: timestamp(row.try_get("expires_unix")?)?,
    })
}

pub(super) async fn register(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RegisterPersonalKey>, JsonRejection>,
) -> Result<Json<PersonalKeyView>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    write_intent(&headers, &identity, &guard.view().csrf_token, &id)?;
    let Json(req) = body.map_err(|_| AuthError::Invalid)?;
    if !req.valid() {
        return Err(AuthError::Invalid.into());
    }
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let tx = guard.transaction();
    let inserted = sqlx::query("WITH instant AS MATERIALIZED (SELECT floor(extract(epoch FROM clock_timestamp()))::bigint AS n)
        INSERT INTO personal_automation_keys(user_id,key_id,owner_id,verifier,scope,label,ttl_seconds,created_unix,expires_unix)
        SELECT $1,$2::text::uuid,$3,$4,$5,$6,$7,n,n+$7::integer FROM instant ON CONFLICT DO NOTHING")
        .bind(&user).bind(&id).bind(&owner).bind(&req.verifier).bind(req.scope.as_str()).bind(&req.label)
        .bind(req.ttl_seconds).execute(&mut **tx).await?;
    // A prior withdrawal has a NULL verifier. A retry must never turn it back into a credential.
    let row=sqlx::query("SELECT key_id::text AS id,user_id,owner_id,scope,label,created_unix,expires_unix,revoked_unix,
        expires_unix<=floor(extract(epoch FROM clock_timestamp())) AS expired
        FROM personal_automation_keys WHERE user_id=$1 AND key_id=$2::text::uuid AND owner_id=$3 AND verifier=$4
        AND scope=$5 AND label=$6 AND ttl_seconds=$7 FOR SHARE")
        .bind(&user).bind(&id).bind(&owner).bind(&req.verifier).bind(req.scope.as_str()).bind(&req.label)
        .bind(req.ttl_seconds).fetch_optional(&mut **tx).await?.ok_or(UserApiError(StatusCode::CONFLICT,"KEY_REGISTRATION_CONFLICT"))?;
    if inserted.rows_affected() == 1 {
        crate::events::append(tx, &owner, &id, "api_key.registered", req.scope.as_str()).await?;
    }
    let result = view(&row)?;
    guard.finish().await?;
    Ok(Json(result))
}

pub(super) async fn revoke(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    write_intent(&headers, &identity, &guard.view().csrf_token, &id)?;
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let changed = sqlx::query(
        "INSERT INTO personal_automation_keys(user_id,key_id,owner_id,revoked_unix)
                 VALUES($1,$2::text::uuid,$3,floor(extract(epoch FROM clock_timestamp())))
                 ON CONFLICT(user_id,key_id) DO UPDATE SET revoked_unix=EXCLUDED.revoked_unix
                 WHERE personal_automation_keys.revoked_unix IS NULL",
    )
    .bind(user)
    .bind(&id)
    .bind(&owner)
    .execute(&mut **guard.transaction())
    .await?;
    if changed.rows_affected() == 1 {
        crate::events::append(
            guard.transaction(),
            &owner,
            &id,
            "api_key.withdrawn",
            "owner_withdrawal",
        )
        .await?;
    }
    guard.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageQuery {
    after: Option<String>,
    limit: Option<u16>,
}

pub(super) async fn list(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<PersonalKeyPage>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let Query(q) = query.map_err(|_| AuthError::Invalid)?;
    let limit = q.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) || q.after.as_deref().is_some_and(|id| !key_id(id)) {
        return Err(AuthError::Invalid.into());
    }
    let user = guard.view().user_id.clone();
    let owner = guard.view().personal_owner_id.clone();
    let rows=sqlx::query("SELECT key_id::text AS id,user_id,owner_id,scope,label,created_unix,expires_unix,revoked_unix,
        expires_unix<=floor(extract(epoch FROM clock_timestamp())) AS expired
        FROM personal_automation_keys WHERE user_id=$1 AND owner_id=$2 AND verifier IS NOT NULL
        AND ($3::text IS NULL OR key_id>$3::text::uuid) ORDER BY key_id LIMIT $4")
        .bind(user).bind(owner).bind(q.after).bind(i64::from(limit)+1).fetch_all(&mut **guard.transaction()).await?;
    let data = rows
        .iter()
        .take(usize::from(limit))
        .map(view)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if rows.len() > usize::from(limit) {
        data.last().map(|k| k.key_id.clone())
    } else {
        None
    };
    guard.finish().await?;
    Ok(Json(PersonalKeyPage { data, next_cursor }))
}

struct KeyRead {
    tx: Transaction<'static, Postgres>,
    user: String,
    owner: String,
    expires: i64,
}
impl KeyRead {
    async fn authorize(
        identity: &UserIdentity,
        headers: &HeaderMap,
        scope: ReadScope,
    ) -> Result<Self, UserApiError> {
        let mut auth = headers.get_all(header::AUTHORIZATION).iter();
        let value = auth
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AuthError::Unauthenticated)?;
        if auth.next().is_some() || value.len() != 105 || headers.contains_key(header::COOKIE) {
            return Err(AuthError::Unauthenticated.into());
        }
        let (id, secret) = value
            .strip_prefix("oa1.")
            .and_then(|t| t.split_once('.'))
            .ok_or(AuthError::Unauthenticated)?;
        if !key_id(id) || !orange_protocol::valid_digest(secret) {
            return Err(AuthError::Unauthenticated.into());
        }
        let mut tx = identity.pool.begin().await?;
        let row=sqlx::query("SELECT k.user_id,k.owner_id,k.scope,k.expires_unix FROM personal_automation_keys k
            JOIN users u ON u.user_id=k.user_id AND u.personal_owner_id=k.owner_id
            JOIN owners o ON o.owner_id=k.owner_id WHERE k.key_id=$1::text::uuid AND k.verifier=$2
            AND k.revoked_unix IS NULL AND k.expires_unix>floor(extract(epoch FROM clock_timestamp())) AND u.active AND o.state='active'
            FOR SHARE OF k,u,o").bind(id).bind(hash_secret(value)).fetch_optional(&mut *tx).await?.ok_or(AuthError::Unauthenticated)?;
        if row.try_get::<&str, _>("scope")? != scope.as_str() {
            return Err(AuthError::Forbidden.into());
        }
        Ok(Self {
            tx,
            user: row.try_get("user_id")?,
            owner: row.try_get("owner_id")?,
            expires: row.try_get("expires_unix")?,
        })
    }
    async fn finish(mut self) -> Result<(), UserApiError> {
        let valid: bool =
            sqlx::query_scalar("SELECT $1::bigint>floor(extract(epoch FROM clock_timestamp()))")
                .bind(self.expires)
                .fetch_one(&mut *self.tx)
                .await?;
        if !valid {
            return Err(AuthError::Unauthenticated.into());
        }
        self.tx.commit().await?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WalletQuery {}

pub(super) async fn wallet(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<WalletQuery>, QueryRejection>,
) -> Result<Json<WalletView>, UserApiError> {
    let mut key = KeyRead::authorize(&identity, &headers, ReadScope::Wallet).await?;
    query.map_err(|_| AuthError::Invalid)?;
    let result = Store::wallet_for_owner(&mut key.tx, &key.user, &key.owner).await?;
    key.finish().await?;
    Ok(Json(result))
}
pub(super) async fn budgets(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<PersonalBudgetList>, UserApiError> {
    let mut key = KeyRead::authorize(&identity, &headers, ReadScope::Budgets).await?;
    let Query(q) = query.map_err(|_| AuthError::Invalid)?;
    let result = Store::budgets_for_owner(
        &mut key.tx,
        &key.user,
        &key.owner,
        q.after.as_deref().unwrap_or(""),
        q.limit.unwrap_or(50),
    )
    .await?;
    key.finish().await?;
    Ok(Json(result))
}
