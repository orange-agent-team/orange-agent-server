use super::*;
impl ArtifactStore {
    pub async fn put<S>(
        &self,
        actor: ArtifactActor,
        owner: String,
        upload: String,
        input: S,
    ) -> Result<ArtifactView, ArtifactError>
    where
        S: Stream<Item = Result<opendal::Buffer, ArtifactError>> + Send + Unpin + 'static,
    {
        let pending = self
            .inner
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let store = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.start_owned(async move {
            let _pending = pending;
            let result = store.put_owned(actor, owner, upload, input).await;
            // A disconnected caller recovers through the original upload ID.
            let _ = send.send(result);
        })
        .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }
    async fn prepare(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
    ) -> Result<Transfer, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        let cap = sqlx::query(
            "SELECT used_bytes,byte_limit FROM artifact_capacity WHERE root_id=$1 FOR UPDATE",
        )
        .bind(&self.inner.objects.root_id)
        .fetch_one(&mut *tx)
        .await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        let worker_output: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM worker_output_uploads WHERE artifact_id=$1)",
        )
        .bind(row.try_get::<String, _>("artifact_id")?)
        .fetch_one(&mut *tx)
        .await?;
        if worker_output {
            return Err(ArtifactError::Forbidden);
        }
        authority::resource(&mut tx, actor, owner, &row, true, false).await?;
        let v = view(&row)?;
        let stamp = authority::stamp(&mut tx, &row).await?;
        if v.state == "deleted" {
            return Err(ArtifactError::Deleted);
        }
        if v.state == "available" {
            let key:String=sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE transfer_id=$1 AND state='attached' AND NOT published_deleted").bind(row.try_get::<Option<String>,_>("published_transfer_id")?).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Integrity)?;
            let result = Transfer {
                id: String::new(),
                stage: String::new(),
                published: key,
                view: v,
                replay: true,
                stamp,
            };
            authority::finish(tx, actor).await?;
            return Ok(result);
        }
        if !row.try_get::<bool, _>("upload_live")? {
            return Err(ArtifactError::Expired);
        }
        // Expired writers remain accounted for and invisible until maintenance.
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifact_transfers WHERE artifact_id=$1 AND state='writing')").bind(&v.artifact_id).fetch_one(&mut *tx).await?;
        if busy {
            return Err(ArtifactError::Busy);
        }
        let bytes = row
            .try_get::<i64, _>("size_bytes")?
            .checked_mul(2)
            .ok_or(ArtifactError::Capacity)?;
        if cap
            .try_get::<i64, _>("used_bytes")?
            .checked_add(bytes)
            .ok_or(ArtifactError::Capacity)?
            > cap.try_get::<i64, _>("byte_limit")?
        {
            return Err(ArtifactError::Capacity);
        }
        let transfer = Transfer {
            id: id(),
            stage: format!("staging-{}", id()),
            published: format!("published-{}", id()),
            view: v,
            replay: false,
            stamp,
        };
        sqlx::query("INSERT INTO artifact_transfers(transfer_id,artifact_id,stage_key,published_key,reserved_bytes,state,expires_at) VALUES($1,$2,$3,$4,$5,'writing',clock_timestamp()+interval '5 minutes')").bind(&transfer.id).bind(&transfer.view.artifact_id).bind(&transfer.stage).bind(&transfer.published).bind(bytes).execute(&mut *tx).await?;
        sqlx::query("UPDATE artifact_capacity SET used_bytes=used_bytes+$2 WHERE root_id=$1")
            .bind(&self.inner.objects.root_id)
            .bind(bytes)
            .execute(&mut *tx)
            .await?;
        authority::finish(tx, actor).await?;
        Ok(transfer)
    }
    async fn put_owned<S>(
        &self,
        actor: ArtifactActor,
        owner: String,
        upload: String,
        mut input: S,
    ) -> Result<ArtifactView, ArtifactError>
    where
        S: Stream<Item = Result<opendal::Buffer, ArtifactError>> + Send + Unpin,
    {
        let deadline = Instant::now() + Duration::from_secs(300);
        let _permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.inner.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ArtifactError::Expired)?
        .map_err(|_| ArtifactError::Unavailable)?;
        let transfer = self.prepare(&actor, &owner, &upload).await?;
        let mut writer: Option<opendal::Writer> = None;
        let mut published: Option<opendal::Writer> = None;
        let result = async {
            let expected = transfer
                .view
                .size_bytes
                .parse::<u64>()
                .map_err(|_| ArtifactError::Integrity)?;
            writer = if transfer.replay {
                None
            } else {
                Some(self.inner.objects.writer(&transfer.stage).await?)
            };
            let mut count = 0u64;
            let mut hash = Sha256::new();
            loop {
                let next =
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), input.next())
                        .await
                        .map_err(|_| ArtifactError::Expired)?;
                let Some(chunk) = next else { break };
                let chunk = chunk?;
                count = count
                    .checked_add(chunk.len() as u64)
                    .ok_or(ArtifactError::Integrity)?;
                if count > expected {
                    return Err(ArtifactError::Integrity);
                }
                for part in chunk.clone() {
                    hash.update(&part);
                }
                if let Some(w) = &mut writer {
                    w.write(chunk).await?;
                }
                if Instant::now() >= deadline {
                    return Err(ArtifactError::Expired);
                }
            }
            if count != expected || hex::encode(hash.finalize()) != transfer.view.sha256 {
                return Err(ArtifactError::Integrity);
            }
            if let Some(mut w) = writer.take() {
                let receipt = w.close().await?;
                self.inner
                    .objects
                    .finish_write(&transfer.stage, receipt)
                    .await?;
            }
            if transfer.replay {
                let stored = self
                    .inner
                    .objects
                    .read(&transfer.published, expected)
                    .await?;
                let mut stored_hash = Sha256::new();
                for part in stored {
                    stored_hash.update(&part);
                }
                if hex::encode(stored_hash.finalize()) != transfer.view.sha256 {
                    return Err(ArtifactError::Integrity);
                }
            }
            if !transfer.replay
                && !self
                    .inner
                    .objects
                    .copy_pinned(
                        &transfer.stage,
                        &transfer.published,
                        expected,
                        &transfer.view.sha256,
                    )
                    .await?
            {
                published = Some(self.inner.objects.writer(&transfer.published).await?);
                let reader = self.inner.objects.reader(&transfer.stage).await?;
                let mut hash = Sha256::new();
                let mut offset = 0;
                while offset < expected {
                    if Instant::now() >= deadline {
                        return Err(ArtifactError::Expired);
                    }
                    self.inner.objects.check_file(&transfer.stage)?;
                    let end = (offset + 64 * 1024).min(expected);
                    let chunk = reader.read(offset..end).await?;
                    if chunk.len() as u64 != end - offset {
                        return Err(ArtifactError::Integrity);
                    }
                    for part in chunk.clone() {
                        hash.update(&part);
                    }
                    published
                        .as_mut()
                        .ok_or(ArtifactError::Unavailable)?
                        .write(chunk)
                        .await?;
                    offset = end;
                }
                let receipt = published
                    .take()
                    .ok_or(ArtifactError::Unavailable)?
                    .close()
                    .await?;
                self.inner
                    .objects
                    .finish_write(&transfer.published, receipt)
                    .await?;
                if hex::encode(hash.finalize()) != transfer.view.sha256 {
                    return Err(ArtifactError::Integrity);
                }
            }
            if Instant::now() >= deadline {
                return Err(ArtifactError::Expired);
            }
            self.publish(&actor, &transfer, deadline).await
        }
        .await;
        // Settle native filesystem operations even after malformed input or a
        // cancelled body. The owned task retains its I/O slot until close ends.
        let mut close_error = None;
        for (pending, key) in [
            (writer.take(), &transfer.stage),
            (published.take(), &transfer.published),
        ] {
            if let Some(mut writer) = pending {
                let closed = match writer.close().await {
                    Ok(meta) => self.inner.objects.finish_write(key, meta).await,
                    Err(e) => Err(ArtifactError::Storage(e)),
                };
                if let Err(error) = closed {
                    close_error = Some(error);
                }
            }
        }
        if result.is_err() && !transfer.replay {
            // Failure of this update leaves 'writing', which is also invisible
            // and accounted for. Do not turn an uncertain DB write into success.
            sqlx::query("UPDATE artifact_transfers SET state='orphan',finished_at=clock_timestamp() WHERE transfer_id=$1 AND state='writing'").bind(&transfer.id).execute(&self.inner.pool).await?;
        }
        if let Some(error) = close_error {
            return Err(error);
        }
        result
    }
    async fn publish(
        &self,
        actor: &ArtifactActor,
        t: &Transfer,
        deadline: Instant,
    ) -> Result<ArtifactView, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, &t.view.owner_id, true).await?;
        let row = self
            .row_in(&mut tx, &t.view.owner_id, &t.view.upload_id)
            .await?;
        authority::resource(&mut tx, actor, &t.view.owner_id, &row, true, false).await?;
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        let current = view(&row)?;
        if authority::stamp(&mut tx, &row).await? != t.stamp {
            return Err(ArtifactError::Forbidden);
        }
        if current.artifact_id != t.view.artifact_id
            || current.version_id != t.view.version_id
            || current.sha256 != t.view.sha256
        {
            return Err(ArtifactError::Integrity);
        }
        if current.state == "deleted" {
            return Err(ArtifactError::Deleted);
        }
        if t.replay {
            if current.state != "available" {
                return Err(ArtifactError::Conflict);
            }
            authority::finish(tx, actor).await?;
            return Ok(current);
        }
        if current.state != "staging" {
            return Err(ArtifactError::Conflict);
        }
        let live:Option<String>=sqlx::query_scalar("UPDATE artifact_transfers SET state='attached',finished_at=clock_timestamp() WHERE transfer_id=$1 AND artifact_id=$2 AND state='writing' AND expires_at>clock_timestamp() RETURNING transfer_id").bind(&t.id).bind(&current.artifact_id).fetch_optional(&mut *tx).await?;
        if live.is_none() {
            return Err(ArtifactError::Expired);
        }
        sqlx::query("UPDATE artifacts SET state='available',published_transfer_id=$2,published_at=clock_timestamp() WHERE artifact_id=$1").bind(&current.artifact_id).bind(&t.id).execute(&mut *tx).await?;
        crate::events::append(
            &mut tx,
            &current.owner_id,
            &current.artifact_id,
            "artifact.available",
            &current.sha256,
        )
        .await?;
        authority::finish(tx, actor).await?;
        let mut result = current;
        result.state = "available".into();
        Ok(result)
    }
    pub async fn read(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
    ) -> Result<(ArtifactView, opendal::Buffer, ReadStamp), ArtifactError> {
        let pending = self
            .inner
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let store = self.clone();
        let actor = actor.clone();
        let owner = owner.to_owned();
        let upload = upload.to_owned();
        let version = version.to_owned();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.start_owned(async move {
            let _pending = pending;
            let result = store.read_owned(&actor, &owner, &upload, &version).await;
            let _ = send.send(result);
        })
        .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }
    async fn read_owned(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
    ) -> Result<(ArtifactView, opendal::Buffer, ReadStamp), ArtifactError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let _permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.inner.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ArtifactError::Expired)?
        .map_err(|_| ArtifactError::Unavailable)?;
        let (v, stamp, key) = self.read_descriptor(actor, owner, upload, version).await?;
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        let data = self
            .inner
            .objects
            .read(
                &key,
                v.size_bytes.parse().map_err(|_| ArtifactError::Integrity)?,
            )
            .await?;
        let mut hash = Sha256::new();
        for part in data.clone() {
            hash.update(&part);
        }
        if hex::encode(hash.finalize()) != v.sha256 {
            return Err(ArtifactError::Integrity);
        }
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let current = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &current, false, false).await?;
        if authority::stamp(&mut tx, &current).await? != stamp {
            return Err(ArtifactError::Forbidden);
        }
        if view(&current)? != v {
            return Err(ArtifactError::Conflict);
        }
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        authority::finish(tx, actor).await?;
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        Ok((v, data, stamp))
    }
    pub(super) async fn read_descriptor(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
    ) -> Result<(ArtifactView, ReadStamp, String), ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        let v = view(&row)?;
        let stamp = authority::stamp(&mut tx, &row).await?;
        if v.state != "available" {
            return Err(if v.state == "deleted" {
                ArtifactError::Deleted
            } else {
                ArtifactError::NotReady
            });
        }
        if v.version_id != version {
            return Err(ArtifactError::Conflict);
        }
        let key:String=sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE transfer_id=$1 AND artifact_id=$2 AND state='attached' AND NOT published_deleted").bind(row.try_get::<Option<String>,_>("published_transfer_id")?).bind(&v.artifact_id).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Integrity)?;
        authority::finish(tx, actor).await?;
        Ok((v, stamp, key))
    }
    pub async fn check_read(
        &self,
        actor: &ArtifactActor,
        expected: &ArtifactView,
        stamp: &ReadStamp,
    ) -> Result<(), ArtifactError> {
        self.check_read_scope(actor, expected, stamp, false).await
    }
    pub(super) async fn check_read_scope(
        &self,
        actor: &ArtifactActor,
        expected: &ArtifactView,
        stamp: &ReadStamp,
        write_session: bool,
    ) -> Result<(), ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, &expected.owner_id, write_session).await?;
        let row = self
            .row_in(&mut tx, &expected.owner_id, &expected.upload_id)
            .await?;
        authority::resource(&mut tx, actor, &expected.owner_id, &row, false, false).await?;
        if view(&row)? != *expected
            || expected.state != "available"
            || authority::stamp(&mut tx, &row).await? != *stamp
        {
            return Err(ArtifactError::Forbidden);
        }
        authority::finish(tx, actor).await
    }
}
