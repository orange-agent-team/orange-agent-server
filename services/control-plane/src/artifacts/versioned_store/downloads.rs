use super::*;

fn now_seconds() -> Result<u64, ArtifactError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ArtifactError::Unavailable)?
        .as_secs())
}
impl VersionedStore {
    async fn sign_get(
        &self,
        key: &str,
        version: &str,
        view: &ArtifactView,
        cutoff: u64,
    ) -> Result<DownloadAuthorization, ArtifactError> {
        let mut url = self
            .bucket_url
            .join(&format!("{}/{}", self.prefix, key))
            .map_err(|_| ArtifactError::StoragePolicy)?;
        url.query_pairs_mut()
            .append_pair("versionId", version)
            .append_pair("response-content-disposition", "attachment")
            .append_pair("response-content-type", "application/octet-stream")
            .append_pair("response-cache-control", "no-store");
        for _ in 0..2 {
            let now = now_seconds()?;
            let ttl = cutoff.saturating_sub(now);
            if ttl == 0 {
                return Err(ArtifactError::Expired);
            }
            if ttl > 60 {
                return Err(ArtifactError::Unavailable);
            }
            let req = Request::builder()
                .method(Method::GET)
                .uri(url.as_str())
                .body(())
                .map_err(|_| ArtifactError::StoragePolicy)?;
            let (mut parts, _) = req.into_parts();
            self.signer
                .sign(&mut parts, Some(Duration::from_secs(ttl)))
                .await
                .map_err(|_| ArtifactError::Unavailable)?;
            // Bound even a retry crossing a wall-clock second to the original grant deadline.
            if now_seconds()? == now {
                return Ok(DownloadAuthorization {
                    artifact_id: view.artifact_id.clone(),
                    version_id: view.version_id.clone(),
                    sha256: view.sha256.clone(),
                    size_bytes: view.size_bytes.clone(),
                    url: parts.uri.to_string(),
                    expires_at_unix: cutoff.to_string(),
                });
            }
        }
        Err(ArtifactError::Unavailable)
    }
}
impl ArtifactStore {
    pub(in crate::artifacts) async fn authorize_download(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
        request: &str,
    ) -> Result<DownloadAuthorization, ArtifactError> {
        if !valid_id(upload) || !valid_id(version) || !valid_id(request) {
            return Err(ArtifactError::Invalid);
        }
        let (source, stamp, key) = self.read_descriptor(actor, owner, upload, version).await?;
        let remote = self.inner.objects.remote()?;
        let pin = remote.checked_version(&key, &source).await;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        if view(&row)? != source || authority::stamp(&mut tx, &row).await? != stamp {
            return Err(ArtifactError::Forbidden);
        }
        let delivered:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM worker_output_uploads w WHERE w.artifact_id=$1)
            OR EXISTS(SELECT 1 FROM worker_output_uploads w JOIN jobs j USING(job_id) JOIN job_response_commits c USING(job_id)
                WHERE w.artifact_id=$1 AND j.owner_id=$2 AND j.execution_state='succeeded' AND c.artifact_id=$1 AND c.version_id=$3)")
            .bind(&source.artifact_id).bind(owner).bind(version).fetch_one(&mut *tx).await?;
        if !delivered {
            return Err(ArtifactError::NotReady);
        }
        let pin = pin?;
        let latest = i64::try_from(now_seconds()? + 60).map_err(|_| ArtifactError::Integrity)?;
        sqlx::query("INSERT INTO artifact_download_grants(session_id,request_key,artifact_id,version_id,owner_id,requester_id,expires_unix)
            VALUES($1,$2,$3,$4,$5,$6,LEAST(floor(extract(epoch FROM clock_timestamp()))::bigint+60,$7)) ON CONFLICT DO NOTHING")
            .bind(&actor.session).bind(request).bind(&source.artifact_id).bind(version).bind(owner).bind(&actor.user).bind(latest).execute(&mut *tx).await?;
        let cutoff:Option<i64>=sqlx::query_scalar("SELECT expires_unix FROM artifact_download_grants WHERE session_id=$1 AND request_key=$2 AND artifact_id=$3
            AND version_id=$4 AND owner_id=$5 AND requester_id=$6")
            .bind(&actor.session).bind(request).bind(&source.artifact_id).bind(version).bind(owner).bind(&actor.user).fetch_optional(&mut *tx).await?;
        let cutoff: u64 = cutoff
            .ok_or(ArtifactError::Conflict)?
            .try_into()
            .map_err(|_| ArtifactError::Integrity)?;
        let grant = remote.sign_get(&key, &pin, &source, cutoff).await?;
        authority::finish(tx, actor).await?;
        if now_seconds()? >= cutoff {
            return Err(ArtifactError::Expired);
        }
        Ok(grant)
    }
}
