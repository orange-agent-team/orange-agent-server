use sqlx::{Postgres, Transaction};
/// Preserve role checks through a transaction that signs or activates a leaf.
pub(crate) async fn current_manager(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    user: &str,
) -> Result<bool, sqlx::Error> {
    let personal: Option<String> = sqlx::query_scalar(
        "SELECT u.personal_owner_id FROM users u JOIN owners o ON o.owner_id=$1
        WHERE u.user_id=$2 AND u.active AND o.state='active' FOR SHARE OF u,o",
    )
    .bind(owner)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(personal) = personal else {
        return Ok(false);
    };
    if personal == owner {
        return Ok(true);
    }
    sqlx::query("SELECT organization_id FROM organizations WHERE organization_id=$1 FOR SHARE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT role FROM memberships WHERE organization_id=$1 AND user_id=$2 AND active FOR SHARE",
    )
    .bind(owner)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .is_some_and(|v| v == "owner" || v == "admin"))
}
