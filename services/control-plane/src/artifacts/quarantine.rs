//! Quarantined third-party bytes have no public artifact upload ID. The lifecycle is anchored in PG.
use super::*;
use crate::confluence::Attachment;
use sha2::{Digest, Sha256};

pub(crate) struct QuarantineRequest<'a> {
    pub run: &'a str,
    pub owner: &'a str,
    pub grant: &'a str,
    pub request: &'a str,
    pub space: &'a str,
    pub attachment: &'a Attachment,
}
impl ArtifactStore {
    pub(crate) async fn reserve_quarantine(
        &self,
        tx: &mut Tx<'_>,
        q: QuarantineRequest<'_>,
    ) -> Result<String, ArtifactError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ArtifactError::Unavailable);
        }
        let n = i64::try_from(q.attachment.bytes).map_err(|_| ArtifactError::Invalid)?;
        if n > crate::confluence::MAX_FILE as i64 {
            return Err(ArtifactError::Invalid);
        }
        let root = &self.inner.objects.root_id;
        let capacity=sqlx::query("UPDATE artifact_capacity SET records=records+1,used_bytes=used_bytes+$2 WHERE root_id=$1 AND records<record_limit AND $2<=byte_limit-used_bytes")
            .bind(root).bind(n).execute(&mut **tx).await?;
        if capacity.rows_affected() != 1 {
            return Err(ArtifactError::Capacity);
        }
        let id = id();
        let meta = serde_json::to_value(q.attachment).map_err(|_| ArtifactError::Invalid)?;
        let staged=sqlx::query("INSERT INTO confluence_quarantine(artifact_id,run_id,owner_id,grant_id,request_key,root_id,space_key,storage_key,meta,size_bytes,state,stage_until,retain_until)
            SELECT $1,$2,$3,$4,$5,$6,$7,'quarantine-'||$1,$8,$9,'staging',c.until_at,g.expires_at FROM repository_source_calls c JOIN external_source_grants g USING(run_id,provider,grant_id)
            WHERE c.run_id=$2 AND c.provider='confluence' AND c.grant_id=$4 AND c.request_key=$5 AND c.finished_at IS NULL AND c.until_at>clock_timestamp()")
            .bind(&id).bind(q.run).bind(q.owner).bind(q.grant).bind(q.request).bind(root).bind(q.space).bind(meta).bind(n).execute(&mut **tx).await?;
        if staged.rows_affected() != 1 {
            return Err(ArtifactError::NotReady);
        }
        Ok(id)
    }
    pub(crate) async fn stage_quarantine(
        &self,
        id: &str,
        meta: &Attachment,
        data: Vec<u8>,
        digest: &str,
    ) -> Result<(), ArtifactError> {
        if !valid_id(id)
            || data.len() as u64 != meta.bytes
            || hex::encode(Sha256::digest(&data)) != digest
        {
            return Err(ArtifactError::Integrity);
        }
        let mut tx = self.inner.pool.begin().await?;
        let key:String=sqlx::query_scalar("SELECT q.storage_key FROM confluence_quarantine q JOIN confluence_quarantine_authority a USING(artifact_id) WHERE q.artifact_id=$1 AND q.root_id=$2 AND q.state='staging' AND q.meta=$3 AND q.stage_until>clock_timestamp() FOR UPDATE OF q")
            .bind(id).bind(&self.inner.objects.root_id).bind(serde_json::to_value(meta).map_err(|_|ArtifactError::Invalid)?).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotReady)?;
        let store = &self.inner.objects;
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut writer = store.writer(&key).await?;
            writer.write(data).await?;
            writer.close().await?;
            store.finish_write(&key)?;
            let verified = store.read(&key, meta.bytes).await?;
            if hex::encode(Sha256::digest(verified.to_bytes())) != digest {
                return Err(ArtifactError::Integrity);
            }
            Ok::<_, ArtifactError>(())
        })
        .await
        .map_err(|_| ArtifactError::Expired)??;
        sqlx::query(
            "UPDATE confluence_quarantine SET state='staged',sha256=$2 WHERE artifact_id=$1",
        )
        .bind(id)
        .bind(digest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn publish_quarantine(
        &self,
        tx: &mut Tx<'_>,
        id: &str,
    ) -> Result<(), ArtifactError> {
        let changed=sqlx::query("UPDATE confluence_quarantine q SET state='available' WHERE q.artifact_id=$1 AND q.root_id=$2 AND q.state='staged' AND q.stage_until>clock_timestamp() AND EXISTS(SELECT FROM confluence_quarantine_authority a WHERE a.artifact_id=q.artifact_id)")
            .bind(id).bind(&self.inner.objects.root_id).execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(ArtifactError::NotReady);
        }
        Ok(())
    }
    pub(crate) async fn discard_quarantine(
        &self,
        id: &str,
        run: &str,
        grant: &str,
    ) -> Result<(), ArtifactError> {
        sqlx::query("UPDATE confluence_quarantine SET state='purging' WHERE artifact_id=$1 AND run_id=$2 AND grant_id=$3 AND root_id=$4 AND state IN ('staging','staged','available')").bind(id).bind(run).bind(grant).bind(&self.inner.objects.root_id).execute(&self.inner.pool).await?;
        Ok(())
    }
    pub(crate) async fn read_quarantine(
        &self,
        actor: &ArtifactActor,
        id: &str,
        run: &str,
        grant: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        let row=sqlx::query("SELECT q.storage_key,q.size_bytes,q.sha256,q.owner_id FROM confluence_quarantine q JOIN confluence_quarantine_authority a USING(artifact_id) JOIN cloud_runs r USING(run_id) WHERE r.user_id=$5 AND q.artifact_id=$1 AND q.run_id=$2 AND q.grant_id=$3 AND q.root_id=$4 AND q.state='available' FOR SHARE OF q")
            .bind(id).bind(run).bind(grant).bind(&self.inner.objects.root_id).bind(&actor.user).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        authority::scope(&mut tx, actor, row.try_get("owner_id")?, false).await?;
        let n = u64::try_from(row.try_get::<i64, _>("size_bytes")?)
            .map_err(|_| ArtifactError::Integrity)?;
        let bytes = self
            .inner
            .objects
            .read(row.try_get("storage_key")?, n)
            .await?
            .to_vec();
        if hex::encode(Sha256::digest(&bytes)) != row.try_get::<String, _>("sha256")? {
            return Err(ArtifactError::Integrity);
        }
        authority::finish(tx, actor).await?;
        Ok(bytes)
    }
    pub(super) async fn collect_quarantine(&self, limit: u16) -> Result<u64, ArtifactError> {
        let root = &self.inner.objects.root_id;
        let mut tx = self.inner.pool.begin().await?;
        let rows=sqlx::query("SELECT q.artifact_id,q.state FROM confluence_quarantine q WHERE root_id=$1 AND state<>'purged' AND (state='purging' OR (state IN ('staging','staged') AND stage_until<=clock_timestamp()) OR NOT EXISTS(SELECT FROM confluence_quarantine_authority a WHERE a.artifact_id=q.artifact_id)) ORDER BY stage_until LIMIT $2 FOR UPDATE OF q SKIP LOCKED").bind(root).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let mut ids = Vec::new();
        for row in rows {
            let id: String = row.try_get("artifact_id")?;
            if row.try_get::<String, _>("state")? != "purging" {
                sqlx::query(
                    "UPDATE confluence_quarantine SET state='purging' WHERE artifact_id=$1",
                )
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            }
            ids.push(id);
        }
        tx.commit().await?;
        let mut removed = 0;
        for id in ids {
            let mut tx = self.inner.pool.begin().await?;
            if let Some(row)=sqlx::query("SELECT storage_key,size_bytes FROM confluence_quarantine WHERE artifact_id=$1 AND root_id=$2 AND state='purging' FOR UPDATE").bind(&id).bind(root).fetch_optional(&mut *tx).await?{
                self.inner.objects.remove(row.try_get("storage_key")?).await?;
                sqlx::query("UPDATE artifact_capacity SET records=records-1,used_bytes=used_bytes-$2 WHERE root_id=$1").bind(root).bind(row.try_get::<i64,_>("size_bytes")?).execute(&mut *tx).await?;
                sqlx::query("UPDATE confluence_quarantine SET state='purged' WHERE artifact_id=$1").bind(id).execute(&mut *tx).await?;removed+=1;
            }
            tx.commit().await?;
        }
        Ok(removed)
    }
}
