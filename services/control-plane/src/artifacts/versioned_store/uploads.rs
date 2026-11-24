use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;

const PUT_TTL: u64 = 300;
fn now() -> Result<u64, ArtifactError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ArtifactError::Unavailable)?
        .as_secs())
}
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(in crate::artifacts) enum UploadAuthorization {
    Available {
        artifact: ArtifactView,
    },
    Authorized {
        upload_id: String,
        artifact_id: String,
        version_id: String,
        transfer_id: String,
        url: String,
        required_headers: BTreeMap<String, String>,
        expires_at_unix: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::artifacts) struct ConfirmUpload {
    pub transfer_id: String,
    pub version_id: String,
    pub provider_version: String,
}
#[derive(Serialize)]
pub(in crate::artifacts) struct UploadObservation {
    upload_id: String,
    artifact_id: String,
    version_id: String,
    transfer_id: String,
    request_id: String,
    state: &'static str,
    provider_version: Option<String>,
    expires_at_unix: String,
}
struct Grant {
    t: Transfer,
    until: u64,
    phase: String,
    ver: Option<String>,
    confirm_live: bool,
    request_key: String,
}

mod provider;
mod retirement;

impl ArtifactStore {
    async fn direct_scope(
        &self,
        tx: &mut Tx<'_>,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        transfer: &str,
        write: bool,
    ) -> Result<Grant, ArtifactError> {
        authority::scope(tx, actor, owner, write).await?;
        let row = self.row_in(tx, owner, upload).await?;
        authority::resource(tx, actor, owner, &row, write, false).await?;
        let view = view(&row)?;
        if view.state == "deleted" {
            return Err(ArtifactError::Deleted);
        }
        let stamp = authority::stamp(tx, &row).await?;
        let grant=sqlx::query("SELECT g.*,t.stage_key,t.published_key,t.state,t.expires_at>clock_timestamp() AS live,c.source_version
            FROM artifact_s3_upload_grants g JOIN artifact_transfers t USING(transfer_id) LEFT JOIN artifact_s3_confirmations c USING(transfer_id)
            WHERE g.transfer_id=$1 AND t.artifact_id=$2 AND g.upload_id=$3 AND g.owner_id=$4 AND g.requester_id=$5 FOR SHARE OF t")
            .bind(transfer).bind(&view.artifact_id).bind(upload).bind(owner).bind(&actor.user).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        if stamp.organization != grant.try_get("organization_version")?
            || stamp.acl != grant.try_get("acl_version")?
        {
            return Err(ArtifactError::Forbidden);
        }
        let phase: String = grant.try_get("state")?;
        if phase == "attached"
            && (view.state != "available"
                || row.try_get::<Option<&str>, _>("published_transfer_id")? != Some(transfer))
        {
            return Err(ArtifactError::Conflict);
        }
        Ok(Grant {
            t: Transfer {
                id: transfer.into(),
                stage: grant.try_get("stage_key")?,
                published: grant.try_get("published_key")?,
                replay: false,
                view,
                stamp,
            },
            until: grant
                .try_get::<i64, _>("expires_unix")?
                .try_into()
                .map_err(|_| ArtifactError::Integrity)?,
            ver: grant.try_get("source_version")?,
            confirm_live: grant.try_get("live")?,
            phase,
            request_key: grant.try_get("request_key")?,
        })
    }
    pub(in crate::artifacts) async fn direct_upload(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
        key: &str,
    ) -> Result<UploadAuthorization, ArtifactError> {
        if !valid_id(upload) || !valid_id(version) || !valid_id(key) {
            return Err(ArtifactError::Invalid);
        }
        let remote = self.inner.objects.remote()?;
        remote.writer_authority().await?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        let lock = request_digest("artifact.s3.upload-key.v1", &(owner, &actor.user, key))
            .map_err(|_| ArtifactError::Invalid)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,89))")
            .bind(lock)
            .execute(&mut *tx)
            .await?;
        let existing:Option<String>=sqlx::query_scalar("SELECT transfer_id FROM artifact_s3_upload_grants WHERE owner_id=$1 AND requester_id=$2 AND request_key=$3")
            .bind(owner).bind(&actor.user).bind(key).fetch_optional(&mut *tx).await?;
        let (t, until) = if let Some(id) = existing {
            let g = self
                .direct_scope(&mut tx, actor, owner, upload, &id, true)
                .await?;
            if g.t.view.version_id != version {
                return Err(ArtifactError::Conflict);
            }
            if g.phase == "attached" {
                authority::finish(tx, actor).await?;
                return Ok(UploadAuthorization::Available { artifact: g.t.view });
            }
            if g.phase != "writing" || !g.confirm_live {
                return Err(ArtifactError::Expired);
            }
            (g.t, g.until)
        } else {
            let t = self
                .prepare_in(&mut tx, actor, owner, upload, 420.0)
                .await?;
            if t.view.version_id != version {
                return Err(ArtifactError::Conflict);
            }
            if t.replay {
                authority::finish(tx, actor).await?;
                return Ok(UploadAuthorization::Available { artifact: t.view });
            }
            let cutoff:i64=sqlx::query_scalar("INSERT INTO artifact_s3_upload_grants(transfer_id,owner_id,upload_id,requester_id,request_key,organization_version,acl_version,expires_unix)
                VALUES($1,$2,$3,$4,$5,$6,$7,LEAST(floor(extract(epoch FROM clock_timestamp()))::bigint+300,$8)) RETURNING expires_unix")
                .bind(&t.id).bind(owner).bind(upload).bind(&actor.user).bind(key).bind(t.stamp.organization).bind(t.stamp.acl)
                .bind(i64::try_from(now()?+PUT_TTL).map_err(|_|ArtifactError::Integrity)?).fetch_one(&mut *tx).await?;
            (t, cutoff.try_into().map_err(|_| ArtifactError::Integrity)?)
        };
        let result = remote.sign_staging(&t, until).await?;
        authority::finish(tx, actor).await?;
        if now()? >= until {
            return Err(ArtifactError::Expired);
        }
        Ok(result)
    }
    pub(in crate::artifacts) async fn latest_upload(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
    ) -> Result<UploadObservation, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        let id:String=sqlx::query_scalar("SELECT g.transfer_id FROM artifact_s3_upload_grants g JOIN artifact_transfers t USING(transfer_id)
            WHERE g.owner_id=$1 AND g.upload_id=$2 AND g.requester_id=$3 AND t.artifact_id=$4 ORDER BY t.created_at DESC,g.transfer_id DESC LIMIT 1")
            .bind(owner).bind(upload).bind(&actor.user).bind(row.try_get::<&str,_>("artifact_id")?).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotFound)?;
        authority::finish(tx, actor).await?;
        self.observe_upload(actor, owner, upload, &id).await
    }

    pub(in crate::artifacts) async fn observe_upload(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        transfer: &str,
    ) -> Result<UploadObservation, ArtifactError> {
        if !valid_id(transfer) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let initial = self
            .direct_scope(&mut tx, actor, owner, upload, transfer, false)
            .await?;
        authority::finish(tx, actor).await?;
        let observed =
            if initial.phase == "writing" && initial.confirm_live && initial.ver.is_none() {
                self.inner
                    .objects
                    .remote()?
                    .observe_stage(&initial.t, None)
                    .await
                    .map(|o| o.and_then(|m| m.version().map(str::to_owned)))
            } else {
                Ok(None)
            };
        let mut tx = self.inner.pool.begin().await?;
        let g = self
            .direct_scope(&mut tx, actor, owner, upload, transfer, false)
            .await?;
        authority::finish(tx, actor).await?;
        let (phase, v) = match g.phase.as_str() {
            "attached" => ("available", g.ver),
            "writing" if g.confirm_live && g.ver.is_some() => ("verifying", g.ver),
            "writing" if g.confirm_live => match observed? {
                Some(v) => ("ready_to_confirm", Some(v)),
                None => ("unconfirmed", None),
            },
            _ => ("expired", None),
        };
        Ok(UploadObservation {
            upload_id: upload.into(),
            artifact_id: g.t.view.artifact_id,
            version_id: g.t.view.version_id,
            transfer_id: transfer.into(),
            request_id: g.request_key,
            state: phase,
            provider_version: v,
            expires_at_unix: g.until.to_string(),
        })
    }
    pub(in crate::artifacts) async fn confirm_direct(
        &self,
        actor: ArtifactActor,
        owner: String,
        upload: String,
        request: ConfirmUpload,
    ) -> Result<ArtifactView, ArtifactError> {
        if !valid_id(&request.transfer_id)
            || !valid_id(&request.version_id)
            || !safe_version(&request.provider_version)
        {
            return Err(ArtifactError::Invalid);
        }
        let store = self.clone();
        let pending = self
            .inner
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| ArtifactError::Busy)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.start_owned(async move {
            let _pending = pending;
            let _ = send.send(
                store
                    .confirm_direct_owned(&actor, &owner, &upload, &request)
                    .await,
            );
        })
        .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }

    async fn confirm_direct_owned(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        request: &ConfirmUpload,
    ) -> Result<ArtifactView, ArtifactError> {
        let deadline = Instant::now() + Duration::from_secs(90);
        let _slot = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.inner.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ArtifactError::Expired)?
        .map_err(|_| ArtifactError::Unavailable)?;
        let mut tx = self.inner.pool.begin().await?;
        let grant = self
            .direct_scope(&mut tx, actor, owner, upload, &request.transfer_id, true)
            .await?;
        if grant.t.view.version_id != request.version_id
            || grant
                .ver
                .as_ref()
                .is_some_and(|v| v != &request.provider_version)
        {
            return Err(ArtifactError::Conflict);
        }
        if grant.phase == "attached" && grant.ver.is_some() {
            authority::finish(tx, actor).await?;
            return Ok(grant.t.view);
        }
        if !grant.confirm_live || grant.phase != "writing" {
            return Err(ArtifactError::Expired);
        }
        sqlx::query("INSERT INTO artifact_s3_confirmations(transfer_id,source_version) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(&request.transfer_id).bind(&request.provider_version).execute(&mut *tx).await?;
        let pinned: String = sqlx::query_scalar(
            "SELECT source_version FROM artifact_s3_confirmations WHERE transfer_id=$1",
        )
        .bind(&request.transfer_id)
        .fetch_one(&mut *tx)
        .await?;
        if pinned != request.provider_version {
            return Err(ArtifactError::Conflict);
        }
        authority::finish(tx, actor).await?;
        self.inner
            .objects
            .remote()?
            .accept_direct_copy(&grant.t, &pinned)
            .await?;
        self.publish(actor, &grant.t, deadline).await
    }
}
