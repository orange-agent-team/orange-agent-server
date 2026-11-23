use super::*;

/// Caller owns the root capacity lock. Lock a bounded set of candidate artifacts, then
/// recheck eligibility in a new statement after locking. No object I/O inside the transaction.
pub(super) async fn expire_available(
    tx: &mut Tx<'_>,
    root: &str,
    limit: u16,
) -> Result<i64, ArtifactError> {
    let candidates:Vec<String>=sqlx::query_scalar("SELECT a.artifact_id FROM artifacts a
        JOIN (SELECT artifact_id FROM artifact_retention_clocks WHERE anchor_at<=statement_timestamp()-interval '24 hours'
              UNION SELECT artifact_id FROM job_response_commits WHERE committed_at<=statement_timestamp()-interval '24 hours') older
            ON older.artifact_id=a.artifact_id
        CROSS JOIN LATERAL orange_artifact_retention(a.artifact_id) r
        WHERE a.root_id=$1 AND a.state='available' AND NOT r.protected_by_reference AND r.due_at<=clock_timestamp()
        ORDER BY r.due_at,a.artifact_id LIMIT $2 FOR UPDATE OF a SKIP LOCKED")
        .bind(root).bind(i64::from(limit)).fetch_all(&mut **tx).await?;
    let mut count = 0;
    for id in candidates {
        let Some(row)=sqlx::query("INSERT INTO artifact_retention_expirations(artifact_id,version_id,basis,due_at)
            SELECT a.artifact_id,a.version_id,r.basis,r.due_at FROM artifacts a CROSS JOIN LATERAL orange_artifact_retention(a.artifact_id) r
            WHERE a.artifact_id=$1 AND a.state='available' AND r.basis IN ('published','migration_grace','last_reference_released','response_commit')
              AND NOT r.protected_by_reference AND r.due_at<=clock_timestamp() RETURNING version_id")
            .bind(&id).fetch_optional(&mut **tx).await? else {continue};
        let version: String = row.try_get("version_id")?;
        let (owner, resource): (String, Option<String>) = sqlx::query_as(
            "UPDATE artifacts SET state='deleted',deleted_at=clock_timestamp()
            WHERE artifact_id=$1 RETURNING owner_id,resource_id",
        )
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?;
        if let Some(resource) = resource {
            sqlx::query("UPDATE team_resources SET deleted=TRUE,acl_version=acl_version+1 WHERE resource_id=$1")
                .bind(resource).execute(&mut **tx).await?;
        }
        crate::events::append(tx, &owner, &id, "artifact.retention_expired", &version).await?;
        count += 1;
    }
    Ok(count)
}
