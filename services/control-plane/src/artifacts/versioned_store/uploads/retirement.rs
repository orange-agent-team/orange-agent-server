use super::*;
impl VersionedStore {
    pub(in crate::artifacts::versioned_store) async fn retire_direct_stage(
        &self,
        key: &str,
    ) -> Result<bool, ArtifactError> {
        if !key.starts_with("staging-") {
            return Ok(false);
        }
        let deadline:Option<i64>=sqlx::query_scalar("SELECT g.expires_unix FROM artifact_s3_upload_grants g JOIN artifact_transfers t USING(transfer_id)
            JOIN artifacts a USING(artifact_id) WHERE a.root_id=$1 AND t.stage_key=$2")
            .bind(&self.root_id).bind(key).fetch_optional(&self.pool).await?;
        let Some(deadline) = deadline else {
            return Ok(false);
        };
        if (now()? as i64) <= deadline {
            return Err(ArtifactError::Busy);
        }
        let owned = |m: &opendal::Metadata| {
            m.user_metadata().is_some_and(|u| {
                u.get("orange-key").is_some_and(|v| v == key)
                    && u.get("orange-root") == Some(&self.root_id)
            })
        };
        let is_fence = |m: &opendal::Metadata| {
            owned(m)
                && m.content_length() == 0
                && m.user_metadata()
                    .is_some_and(|u| u.get("orange-closed").is_some_and(|v| v == "1"))
        };
        let latest = match self.op.stat(key).await {
            Ok(m) => Some(m),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let marker = match latest {
            Some(m) if is_fence(&m) => m,
            current => {
                let write = self.op.write_with(key, Buffer::new()).user_metadata([
                    ("orange-root".into(), self.root_id.clone()),
                    ("orange-key".into(), key.into()),
                    ("orange-closed".into(), "1".into()),
                ]);
                match current {
                    Some(m) if owned(&m) => {
                        write
                            .if_match(m.etag().ok_or(ArtifactError::StoragePolicy)?)
                            .await?
                    }
                    Some(_) => return Err(ArtifactError::StoragePolicy),
                    None => write.if_not_exists(true).await?,
                }
            }
        };
        let fence = marker
            .version()
            .filter(|v| safe_version(v))
            .ok_or(ArtifactError::StoragePolicy)?;
        let mut list = self
            .op
            .lister_with(key)
            .recursive(true)
            .versions(true)
            .deleted(true)
            .await?;
        let mut retired = 0;
        while let Some(entry) = list.next().await {
            let entry = entry?;
            if entry.path() != key {
                continue;
            }
            let v = entry
                .metadata()
                .version()
                .filter(|v| safe_version(v))
                .ok_or(ArtifactError::StoragePolicy)?;
            if v == fence {
                continue;
            }
            if retired >= 64 {
                return Err(ArtifactError::Busy);
            }
            let meta = self.op.stat_with(key).version(v).await?;
            if !owned(&meta) {
                return Err(ArtifactError::StoragePolicy);
            }
            self.op.delete_with(key).version(v).await?;
            retired += 1;
        }
        let latest = self.op.stat(key).await?;
        if !is_fence(&latest) || latest.version() != Some(fence) {
            return Err(ArtifactError::Busy);
        }
        let mut list = self
            .op
            .lister_with(key)
            .recursive(true)
            .versions(true)
            .deleted(true)
            .await?;
        while let Some(entry) = list.next().await {
            let item = entry?;
            if item.path() == key && item.metadata().version() != Some(fence) {
                return Err(ArtifactError::Busy);
            }
        }
        Ok(true)
    }
}
