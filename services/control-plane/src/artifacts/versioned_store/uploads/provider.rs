use super::*;
impl VersionedStore {
    pub(super) async fn sign_staging(
        &self,
        t: &Transfer,
        until: u64,
    ) -> Result<UploadAuthorization, ArtifactError> {
        key_valid(&t.stage)?;
        let mut headers = BTreeMap::new();
        headers.insert("content-length".into(), t.view.size_bytes.clone());
        headers.insert("if-none-match".into(), "*".into());
        headers.insert(
            "x-amz-checksum-sha256".into(),
            STANDARD.encode(hex::decode(&t.view.sha256).map_err(|_| ArtifactError::Integrity)?),
        );
        headers.insert("x-amz-meta-orange-root".into(), self.root_id.clone());
        headers.insert("x-amz-meta-orange-key".into(), t.stage.clone());
        let url = self
            .bucket_url
            .join(&format!("{}/{}", self.prefix, t.stage))
            .map_err(|_| ArtifactError::StoragePolicy)?;
        for _ in 0..2 {
            let before = now()?;
            let ttl = until.saturating_sub(before);
            if ttl == 0 {
                return Err(ArtifactError::Expired);
            }
            if ttl > PUT_TTL {
                return Err(ArtifactError::Unavailable);
            }
            let mut req = Request::builder().method(Method::PUT).uri(url.as_str());
            for (name, value) in &headers {
                req = req.header(name, value);
            }
            let (mut parts, _) = req
                .body(())
                .map_err(|_| ArtifactError::StoragePolicy)?
                .into_parts();
            self.signer
                .sign(&mut parts, Some(Duration::from_secs(ttl)))
                .await
                .map_err(|_| ArtifactError::Unavailable)?;
            if now()? == before {
                let url = reqwest::Url::parse(&parts.uri.to_string())
                    .map_err(|_| ArtifactError::Integrity)?;
                let signed = url
                    .query_pairs()
                    .find(|(k, _)| k.eq_ignore_ascii_case("x-amz-signedheaders"))
                    .ok_or(ArtifactError::Integrity)?
                    .1;
                if !headers.keys().all(|k| signed.split(';').any(|f| f == k)) {
                    return Err(ArtifactError::StoragePolicy);
                }
                return Ok(UploadAuthorization::Authorized {
                    upload_id: t.view.upload_id.clone(),
                    artifact_id: t.view.artifact_id.clone(),
                    version_id: t.view.version_id.clone(),
                    transfer_id: t.id.clone(),
                    url: url.into(),
                    required_headers: headers,
                    expires_at_unix: until.to_string(),
                });
            }
        }
        Err(ArtifactError::Unavailable)
    }
    pub(super) async fn observe_stage(
        &self,
        t: &Transfer,
        v: Option<&str>,
    ) -> Result<Option<opendal::Metadata>, ArtifactError> {
        key_valid(&t.stage)?;
        self.marker.verify(&self.op).await?;
        let target = self.op.stat_with(&t.stage);
        let result = if let Some(v) = v {
            if !safe_version(v) {
                return Err(ArtifactError::Invalid);
            }
            target.version(v).await
        } else {
            target.await
        };
        let meta = match result {
            Ok(m) => m,
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let actual = meta
            .version()
            .filter(|s| safe_version(s))
            .ok_or(ArtifactError::StoragePolicy)?;
        if v.is_some_and(|v| v != actual)
            || meta.content_length().to_string() != t.view.size_bytes
            || !meta.user_metadata().is_some_and(|m| {
                m.get("orange-root") == Some(&self.root_id) && m.get("orange-key") == Some(&t.stage)
            })
        {
            return Err(ArtifactError::Integrity);
        }
        Ok(Some(meta))
    }
    async fn recover_direct_copy(&self, t: &Transfer, size: u64) -> Result<bool, ArtifactError> {
        // A lost CopyObject response can leave a real target before its PostgreSQL pin is committed.
        let pinned: Option<String> = sqlx::query_scalar(
            "SELECT provider_version FROM artifact_s3_versions WHERE root_id=$1 AND storage_key=$2",
        )
        .bind(&self.root_id)
        .bind(&t.published)
        .fetch_optional(&self.pool)
        .await?;
        let stat = self.op.stat_with(&t.published);
        let result = if let Some(v) = &pinned {
            stat.version(v).await
        } else {
            stat.await
        };
        let meta = match result {
            Ok(m) => m,
            Err(e) if e.kind() == opendal::ErrorKind::NotFound && pinned.is_none() => {
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let v = meta
            .version()
            .filter(|s| safe_version(s))
            .ok_or(ArtifactError::Integrity)?;
        if meta.content_length() != size
            || !meta.user_metadata().is_some_and(|m| {
                m.get("orange-root") == Some(&self.root_id) && m.get("orange-key") == Some(&t.stage)
            })
        {
            return Err(ArtifactError::Integrity);
        }
        let data = self.op.read_with(&t.published).version(v).await?;
        if data.len() as u64 != size
            || hex::encode(Sha256::digest(data.to_bytes())) != t.view.sha256
        {
            return Err(ArtifactError::Integrity);
        }
        self.pin(&t.published, &meta).await?;
        Ok(true)
    }
    pub(super) async fn accept_direct_copy(
        &self,
        t: &Transfer,
        v: &str,
    ) -> Result<(), ArtifactError> {
        let meta = self
            .observe_stage(t, Some(v))
            .await?
            .ok_or(ArtifactError::NotReady)?;
        self.pin(&t.stage, &meta).await?;
        let size = t
            .view
            .size_bytes
            .parse()
            .map_err(|_| ArtifactError::Integrity)?;
        if self.recover_direct_copy(t, size).await? {
            return Ok(());
        }
        if let Err(error) = self
            .copy(&t.stage, &t.published, size, &t.view.sha256)
            .await
            && !self.recover_direct_copy(t, size).await?
        {
            return Err(error);
        }
        Ok(())
    }
}
