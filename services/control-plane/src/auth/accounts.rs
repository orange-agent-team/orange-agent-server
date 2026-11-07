//! Personal account routes: identity determines the owner; no caller-selected
//! account, development bearer fallback, balance mutation or implicit credit.
use super::*;
use crate::store::{Store, StoreError};
use orange_protocol::{
    BudgetView, CreateBudget,
    account::{PersonalBudgetList, WalletView},
};
impl From<StoreError> for UserApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Invalid => AuthError::Invalid.into(),
            StoreError::Forbidden => AuthError::Forbidden.into(),
            StoreError::NotFound => Self(StatusCode::NOT_FOUND, "NOT_FOUND"),
            StoreError::Conflict => Self(StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT"),
            StoreError::Database(_) | StoreError::Migration(_) => AuthError::Unavailable.into(),
            _ => Self(StatusCode::CONFLICT, "ACCOUNT_OPERATION_REJECTED"),
        }
    }
}
pub(super) async fn wallet(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
) -> Result<Json<WalletView>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let result = Store::personal_wallet(&mut guard).await?;
    guard.finish().await?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetQuery {
    after: Option<String>,
    limit: Option<u16>,
}
pub(super) async fn budgets(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    query: Result<Query<BudgetQuery>, QueryRejection>,
) -> Result<Json<PersonalBudgetList>, UserApiError> {
    let Query(query) = query.map_err(|_| AuthError::Invalid)?;
    let mut guard = identity.authenticate(&headers).await?;
    let result = Store::personal_budgets(
        &mut guard,
        query.after.as_deref().unwrap_or(""),
        query.limit.unwrap_or(50),
    )
    .await?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn budget(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    Path(root): Path<String>,
) -> Result<Json<BudgetView>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    let result = Store::personal_budget(&mut guard, &root).await?;
    guard.finish().await?;
    Ok(Json(result))
}
pub(super) async fn create_budget(
    State(identity): State<Arc<UserIdentity>>,
    headers: HeaderMap,
    body: Result<Json<CreateBudget>, JsonRejection>,
) -> Result<Json<BudgetView>, UserApiError> {
    let mut guard = identity.authenticate(&headers).await?;
    check_write(
        &headers,
        &identity.config.user_origin,
        &guard.view().csrf_token,
    )?;
    let key = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .filter(|s| valid_id(s))
        .ok_or(AuthError::Invalid)?;
    let Json(request) = body.map_err(|_| AuthError::Invalid)?;
    let result = Store::create_personal_budget(&mut guard, key, &request).await?;
    guard.finish().await?;
    Ok(Json(result))
}
