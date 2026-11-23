use super::*;
impl ArtifactStore {
    /// Service-owned maintenance, not an end-user deletion shortcut. Taking all
    /// I/O slots ensures an expired database lease cannot reclaim a live writer.
    pub async fn collect(&self, limit: u16) -> Result<u64, ArtifactError> {
        if !(1..=100).contains(&limit) {
            return Err(ArtifactError::Invalid);
        }
        let _exclusive = self
            .inner
            .slots
            .clone()
            .acquire_many_owned(4)
            .await
            .map_err(|_| ArtifactError::Unavailable)?;
        let mut tx = self.inner.pool.begin().await?;
        sqlx::query("SELECT root_id FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
            .bind(&self.inner.objects.root_id)
            .fetch_one(&mut *tx)
            .await?;
        let expired=sqlx::query("SELECT * FROM artifacts a WHERE a.root_id=$1 AND a.state='staging' AND a.created_at<clock_timestamp()-interval '1 hour' AND NOT EXISTS(SELECT 1 FROM artifact_pins p WHERE p.artifact_id=a.artifact_id) ORDER BY a.created_at LIMIT $2 FOR UPDATE OF a SKIP LOCKED")
            .bind(&self.inner.objects.root_id).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        for row in &expired {
            let artifact: String = row.try_get("artifact_id")?;
            sqlx::query("UPDATE artifacts SET state='deleted',deleted_at=clock_timestamp() WHERE artifact_id=$1").bind(&artifact).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO artifact_expirations(artifact_id,reason) VALUES($1,'upload_expired')",
            )
            .bind(&artifact)
            .execute(&mut *tx)
            .await?;
            if let Some(resource) = row.try_get::<Option<String>, _>("resource_id")? {
                sqlx::query("UPDATE team_resources SET deleted=TRUE,acl_version=acl_version+1 WHERE resource_id=$1").bind(resource).execute(&mut *tx).await?;
            }
            crate::events::append(
                &mut tx,
                &row.try_get::<String, _>("owner_id")?,
                &artifact,
                "artifact.upload_expired",
                &row.try_get::<String, _>("version_id")?,
            )
            .await?;
        }
        let elapsed =
            super::retention::expire_available(&mut tx, &self.inner.objects.root_id, limit).await?;
        let derived=sqlx::query("SELECT a.*,i.artifact_id AS source_id FROM worker_output_uploads w JOIN result_source_bindings i USING(job_id) JOIN artifacts source ON source.artifact_id=i.artifact_id JOIN artifacts a ON a.artifact_id=w.artifact_id WHERE a.root_id=$1 AND a.state!='deleted' AND source.state='deleted' ORDER BY a.created_at LIMIT $2 FOR UPDATE OF a SKIP LOCKED").bind(&self.inner.objects.root_id).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        for row in &derived {
            let artifact: String = row.try_get("artifact_id")?;
            sqlx::query("UPDATE artifacts SET state='deleted',deleted_at=clock_timestamp() WHERE artifact_id=$1").bind(&artifact).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO artifact_derivative_deletions(artifact_id,source_artifact_id) VALUES($1,$2)").bind(&artifact).bind(row.try_get::<String,_>("source_id")?).execute(&mut *tx).await?;
            if let Some(resource) = row.try_get::<Option<String>, _>("resource_id")? {
                sqlx::query("UPDATE team_resources SET deleted=TRUE,acl_version=acl_version+1 WHERE resource_id=$1").bind(resource).execute(&mut *tx).await?;
            }
            crate::events::append(
                &mut tx,
                &row.try_get::<String, _>("owner_id")?,
                &artifact,
                "artifact.source_deleted",
                &row.try_get::<String, _>("version_id")?,
            )
            .await?;
        }
        sqlx::query("UPDATE artifact_capacity SET records=records-$2 WHERE root_id=$1")
            .bind(&self.inner.objects.root_id)
            .bind((expired.len() + derived.len()) as i64 + elapsed)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        // No live I/O holds a slot here. Expiry fences an old process/request,
        // while physical orphan retention remains one hour from creation.
        sqlx::query("UPDATE artifact_transfers t SET state='orphan',finished_at=clock_timestamp() FROM artifacts a WHERE t.artifact_id=a.artifact_id AND a.root_id=$1 AND t.state='writing' AND t.expires_at<=clock_timestamp()")
            .bind(&self.inner.objects.root_id).execute(&self.inner.pool).await?;
        let candidates:Vec<String>=sqlx::query_scalar("SELECT t.transfer_id FROM artifact_transfers t JOIN artifacts a USING(artifact_id) WHERE a.root_id=$1 AND t.state!='purged' AND ((t.state='attached' AND NOT t.stage_deleted) OR t.state='purging' OR (t.state IN('writing','orphan') AND t.created_at<clock_timestamp()-interval '1 hour' AND t.expires_at<clock_timestamp()) OR (a.state='deleted' AND orange_deleted_version_due(a.artifact_id))) ORDER BY t.created_at LIMIT $2").bind(&self.inner.objects.root_id).bind(i64::from(limit)).fetch_all(&self.inner.pool).await?;
        let mut cleaned = 0;
        for id in candidates {
            let mut tx = self.inner.pool.begin().await?;
            sqlx::query("SELECT root_id FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
                .bind(&self.inner.objects.root_id)
                .fetch_one(&mut *tx)
                .await?;
            let artifact: String = sqlx::query_scalar(
                "SELECT artifact_id FROM artifact_transfers WHERE transfer_id=$1",
            )
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
            let row=sqlx::query("SELECT a.*,state='deleted' AND orange_deleted_version_due(a.artifact_id) AS delete_ready FROM artifacts a WHERE artifact_id=$1 FOR UPDATE").bind(&artifact).fetch_one(&mut *tx).await?;
            let transfer=sqlx::query("SELECT *,created_at<clock_timestamp()-interval '1 hour' AND expires_at<clock_timestamp() AS orphan_ready FROM artifact_transfers WHERE transfer_id=$1 FOR UPDATE").bind(&id).fetch_one(&mut *tx).await?;
            let state: String = transfer.try_get("state")?;
            let pins: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM artifact_pins WHERE artifact_id=$1)",
            )
            .bind(&artifact)
            .fetch_one(&mut *tx)
            .await?;
            let remove_all = state == "purging"
                || (matches!(state.as_str(), "writing" | "orphan")
                    && transfer.try_get::<bool, _>("orphan_ready")?)
                || (row.try_get::<Option<bool>, _>("delete_ready")? == Some(true) && !pins);
            let stage_only =
                state == "attached" && !transfer.try_get::<bool, _>("stage_deleted")?;
            if state == "purged" || (!remove_all && !stage_only) {
                tx.commit().await?;
                continue;
            }
            // A published reference is never reclaimed while any pin exists.
            if remove_all
                && row
                    .try_get::<Option<String>, _>("published_transfer_id")?
                    .as_deref()
                    == Some(id.as_str())
                && pins
            {
                tx.commit().await?;
                continue;
            }
            let stage: String = transfer.try_get("stage_key")?;
            let published: String = transfer.try_get("published_key")?;
            if remove_all {
                sqlx::query("UPDATE artifact_transfers SET state='purging' WHERE transfer_id=$1")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            // No database transaction spans object I/O. Failed removal retains
            // both accounting and purging state for an idempotent later attempt.
            self.inner.objects.remove(&stage).await?;
            if remove_all {
                self.inner.objects.remove(&published).await?;
            }
            let mut tx = self.inner.pool.begin().await?;
            sqlx::query("SELECT root_id FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
                .bind(&self.inner.objects.root_id)
                .fetch_one(&mut *tx)
                .await?;
            let current =
                sqlx::query("SELECT * FROM artifact_transfers WHERE transfer_id=$1 FOR UPDATE")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?;
            let remaining: i64 = current.try_get("reserved_bytes")?;
            let released = if remove_all {
                remaining
            } else if !current.try_get::<bool, _>("stage_deleted")? {
                row.try_get::<i64, _>("size_bytes")?.min(remaining)
            } else {
                0
            };
            sqlx::query("UPDATE artifact_capacity SET used_bytes=used_bytes-$2 WHERE root_id=$1")
                .bind(&self.inner.objects.root_id)
                .bind(released)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE artifact_transfers SET stage_deleted=TRUE,published_deleted=published_deleted OR $2,reserved_bytes=reserved_bytes-$3,state=CASE WHEN $2 THEN 'purged' ELSE state END WHERE transfer_id=$1").bind(&id).bind(remove_all).bind(released).execute(&mut *tx).await?;
            tx.commit().await?;
            cleaned += 1;
        }
        let remaining = limit.saturating_sub(u16::try_from(cleaned).unwrap_or(u16::MAX));
        Ok(cleaned + self.collect_quarantine(remaining).await?)
    }
}
