use super::*;
use orange_protocol::pricing::{
    CreateTranscriptionInputGrant, TranscriptionGrantReceipt, TranscriptionInputTarget,
    TranscriptionInputWithdrawal,
};

fn receipt(row: &sqlx::postgres::PgRow) -> Result<TranscriptionGrantReceipt, ArtifactError> {
    Ok(TranscriptionGrantReceipt {
        grant_id: row.try_get("grant_id")?,
        invocation_id: row.try_get("invocation_id")?,
        observation_id: row.try_get("observation_id")?,
        projection_digest: row.try_get("projection_digest")?,
        expires_at_unix: row.try_get::<i64, _>("expires")?.to_string(),
        state: row.try_get("grant_state")?,
    })
}

pub(super) async fn lookup(
    tx: &mut Tx<'_>,
    owner: &str,
    id: &str,
    chunk: Option<i32>,
) -> Result<Option<sqlx::postgres::PgRow>, ArtifactError> {
    Ok(sqlx::query("SELECT g.grant_id,g.author_id,g.language,g.invocation_id,g.root_budget_id,g.upload_id,g.region_id,g.observation_id,g.projection_digest,g.request_digest,g.organization_policy,g.acl_version,g.measurement,g.chunk_digests[$3] AS chunk_sha,floor(extract(epoch FROM g.expires_at))::bigint AS expires,
        CASE WHEN x.grant_id IS NOT NULL THEN 'revoked' WHEN g.expires_at<=clock_timestamp() THEN 'expired' ELSE 'active' END AS grant_state
        FROM transcription_input_grants g LEFT JOIN transcription_input_revocations x ON x.owner_id=g.owner_id AND x.grant_id=g.grant_id AND x.revoked_by=g.author_id
        WHERE g.owner_id=$1 AND g.grant_id=$2")
        .bind(owner).bind(id).bind(chunk).fetch_optional(&mut **tx).await?)
}

pub(super) async fn observed_target(
    tx: &mut Tx<'_>,
    id: &str,
) -> Result<sqlx::postgres::PgRow, ArtifactError> {
    let (node,cert):(String,String)=sqlx::query_as("SELECT o.node_id,s.certificate_sha256 FROM worker_transcription_profile_observations o JOIN worker_sessions s
          ON s.node_id=o.node_id AND s.session_id=o.session_id AND s.connection_epoch=o.connection_epoch WHERE o.observation_id=$1")
        .bind(id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)?;
    crate::worker::sessions::authorize(tx, &node, &cert)
        .await
        .map_err(|e| match e {
            crate::worker::SessionError::Database(err) => ArtifactError::Database(err),
            _ => ArtifactError::NotReady,
        })?;
    let row=sqlx::query("SELECT o.node_id,n.region_id,o.model_version,o.session_id,o.connection_epoch,o.execution_profile_id,o.execution_profile_digest,o.observation_digest
        FROM worker_transcription_profile_observations o JOIN reviewed_execution_profiles p USING(execution_profile_id)
        JOIN worker_sessions s ON s.node_id=o.node_id AND s.session_id=o.session_id AND s.connection_epoch=o.connection_epoch
        JOIN nodes n ON n.node_id=s.node_id WHERE o.observation_id=$1 AND n.environment='dev' AND n.state='online' AND s.certificate_sha256=$2
        AND p.enabled AND p.execution_profile_digest=o.execution_profile_digest AND p.model_sha256=o.model_sha256
        AND p.model_version_id=o.model_version AND s.execution_control_version=2 AND s.input_artifacts_version=1
        AND s.expires_at>clock_timestamp() FOR SHARE OF s,p")
        .bind(id).bind(cert).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::NotReady)?;
    Ok(row)
}

async fn consent_lock(tx: &mut Tx<'_>, owner: &str, id: &str) -> Result<(), ArtifactError> {
    let key = request_digest("transcription.consent.lock.v1", &(owner, id))
        .map_err(|_| ArtifactError::Invalid)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn not_withdrawn(
    tx: &mut Tx<'_>,
    actor: &ArtifactActor,
    owner: &str,
    id: &str,
) -> Result<(), ArtifactError> {
    let revoked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transcription_input_revocations WHERE owner_id=$1 AND grant_id=$2 AND revoked_by=$3)")
        .bind(owner).bind(id).bind(&actor.user).fetch_one(&mut **tx).await?;
    if revoked {
        return Err(ArtifactError::Conflict);
    }
    Ok(())
}
#[cfg(unix)]
async fn selected_target(
    tx: &mut Tx<'_>,
    r: &CreateTranscriptionInputGrant,
) -> Result<sqlx::postgres::PgRow, ArtifactError> {
    let target = observed_target(tx, &r.observation_id).await?;
    if target.try_get::<String, _>("node_id")? != r.node_id
        || target.try_get::<String, _>("region_id")? != r.region_id
        || target.try_get::<String, _>("execution_profile_digest")? != r.execution_profile_digest
    {
        return Err(ArtifactError::Conflict);
    }
    Ok(target)
}
impl ArtifactStore {
    pub(super) async fn audio_target(
        &self,
        actor: &ArtifactActor,
        id: &str,
    ) -> Result<TranscriptionInputTarget, ArtifactError> {
        if !valid_id(id) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        let row = observed_target(&mut tx, id).await?;
        let result = TranscriptionInputTarget {
            observation_id: id.to_owned(),
            node_id: row.try_get("node_id")?,
            region_id: row.try_get("region_id")?,
            model_version: row.try_get("model_version")?,
            execution_profile_digest: row.try_get("execution_profile_digest")?,
        };
        authority::finish(tx, actor).await?;
        Ok(result)
    }
    pub(super) async fn authorized_audio_source(
        &self,
        tx: &mut Tx<'_>,
        user: &str,
        owner: &str,
        grant: &sqlx::postgres::PgRow,
    ) -> Result<(ArtifactView, Option<String>), ArtifactError> {
        let role = authority::user_scope(tx, user, owner, true).await?;
        let row = self.row_in(tx, owner, grant.try_get("upload_id")?).await?;
        authority::resource_role(tx, user, owner, &row, role, false, false).await?;
        authority::resource_role(tx, user, owner, &row, role, true, false).await?;
        let stamp = authority::stamp(tx, &row).await?;
        let source = view(&row)?;
        let measured: orange_protocol::pricing::AudioMeasurement =
            serde_json::from_value(grant.try_get("measurement")?)
                .map_err(|_| ArtifactError::Integrity)?;
        if stamp.organization != grant.try_get("organization_policy")?
            || stamp.acl != grant.try_get("acl_version")?
            || source.state != "available"
            || source.artifact_id != measured.artifact_id
            || source.version_id != measured.version_id
            || source.sha256 != measured.audio_sha256
        {
            return Err(ArtifactError::Forbidden);
        }
        Ok((source, row.try_get("published_transfer_id")?))
    }
    pub(super) async fn audio_grant_status(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        id: &str,
    ) -> Result<TranscriptionGrantReceipt, ArtifactError> {
        if !valid_id(id) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        consent_lock(&mut tx, owner, id).await?;
        let row = lookup(&mut tx, owner, id, None)
            .await?
            .ok_or(ArtifactError::NotFound)?;
        if row.try_get::<String, _>("author_id")? != actor.user {
            return Err(ArtifactError::NotFound);
        }
        if row.try_get::<String, _>("grant_state")? == "active" {
            let target = observed_target(&mut tx, row.try_get("observation_id")?).await?;
            if target.try_get::<String, _>("region_id")? != row.try_get::<String, _>("region_id")? {
                return Err(ArtifactError::Forbidden);
            }
            let budget: &str = row.try_get("root_budget_id")?;
            crate::store::check_budget_actor(&mut tx, &actor.user, owner, budget)
                .await
                .map_err(|_| ArtifactError::Forbidden)?;
            sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE").bind(budget).bind(owner).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Forbidden)?;
            self.authorized_audio_source(&mut tx, &actor.user, owner, &row)
                .await?;
        }
        let result = receipt(
            &lookup(&mut tx, owner, id, None)
                .await?
                .ok_or(ArtifactError::NotFound)?,
        )?;
        authority::finish(tx, actor).await?;
        Ok(result)
    }
    #[cfg(unix)]
    pub(super) async fn grant_audio_input(
        &self,
        meter: &AudioMeter,
        actor: &ArtifactActor,
        owner: &str,
        r: &CreateTranscriptionInputGrant,
    ) -> Result<TranscriptionGrantReceipt, ArtifactError> {
        if !r.allow_remote_transcription
            || !valid_id(&r.node_id)
            || !valid_id(&r.region_id)
            || !orange_protocol::valid_digest(&r.execution_profile_digest)
            || !(1..=900).contains(&r.ttl_seconds)
            || ![
                &r.grant_id,
                &r.root_budget_id,
                &r.invocation_id,
                &r.upload_id,
                &r.observation_id,
                &r.measurement.version_id,
            ]
            .iter()
            .all(|s| valid_id(s))
        {
            return Err(ArtifactError::Invalid);
        }
        if let Some(language) = &r.language {
            let config: serde_json::Value = serde_json::from_slice(include_bytes!(
                "../../../../config/local-transcription.lock.json"
            ))
            .map_err(|_| ArtifactError::Integrity)?;
            if !config["language_codes"]
                .as_array()
                .is_some_and(|languages| languages.iter().any(|code| code == language))
            {
                return Err(ArtifactError::Invalid);
            }
        }
        let key = request_digest("transcription.input-consent.v1", r)
            .map_err(|_| ArtifactError::Invalid)?;
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, true).await?;
        consent_lock(&mut tx, owner, &r.grant_id).await?;
        if let Some(row) = lookup(&mut tx, owner, &r.grant_id, None).await? {
            if row.try_get::<String, _>("author_id")? != actor.user
                || row.try_get::<String, _>("request_digest")? != key
            {
                return Err(ArtifactError::Conflict);
            }
            let mut result = receipt(&row)?;
            if result.state == "active" {
                selected_target(&mut tx, r).await?;
                let stamp = self.grant_source(&mut tx, actor, owner, r).await?;
                if stamp.organization != row.try_get("organization_policy")?
                    || stamp.acl != row.try_get("acl_version")?
                {
                    return Err(ArtifactError::Forbidden);
                }
                result = receipt(
                    &lookup(&mut tx, owner, &r.grant_id, None)
                        .await?
                        .ok_or(ArtifactError::NotFound)?,
                )?;
            }
            authority::finish(tx, actor).await?;
            return Ok(result);
        }
        not_withdrawn(&mut tx, actor, owner, &r.grant_id).await?;
        selected_target(&mut tx, r).await?;
        let initial = self.grant_source(&mut tx, actor, owner, r).await?;
        authority::finish(tx, actor).await?;

        let (measured, stamp, chunks) = meter
            .measure_source(
                self,
                actor,
                owner,
                &r.upload_id,
                &r.measurement.version_id,
                true,
            )
            .await?;
        if measured != r.measurement {
            return Err(ArtifactError::Conflict);
        }
        if stamp != initial {
            return Err(ArtifactError::Forbidden);
        }
        let mut tx = self.inner.pool.begin().await?;
        consent_lock(&mut tx, owner, &r.grant_id).await?;
        not_withdrawn(&mut tx, actor, owner, &r.grant_id).await?;
        let target = selected_target(&mut tx, r).await?;
        if self.grant_source(&mut tx, actor, owner, r).await? != initial {
            return Err(ArtifactError::Forbidden);
        }
        let projection=request_digest("transcription.remote-input.v1",&serde_json::json!({
            "owner_id":owner,"user_id":actor.user,"consent":r,"node_id":target.try_get::<String,_>("node_id")?,
            "execution_profile_digest":target.try_get::<String,_>("execution_profile_digest")?,
            "epoch":target.try_get::<i64,_>("connection_epoch")?.to_string(),
            "observation_digest":target.try_get::<String,_>("observation_digest")?
        })).map_err(|_|ArtifactError::Invalid)?;
        sqlx::query("INSERT INTO transcription_input_grants(owner_id,grant_id,author_id,root_budget_id,invocation_id,upload_id,artifact_id,version_id,
            observation_id,language,request_digest,projection_digest,measurement,chunk_digests,organization_policy,acl_version,region_id,expires_at)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,clock_timestamp()+make_interval(secs=>$18)) ON CONFLICT DO NOTHING")
            .bind(owner).bind(&r.grant_id).bind(&actor.user).bind(&r.root_budget_id).bind(&r.invocation_id).bind(&r.upload_id)
            .bind(&measured.artifact_id).bind(&measured.version_id).bind(&r.observation_id).bind(&r.language).bind(&key).bind(&projection)
            .bind(serde_json::to_value(&measured).map_err(|_|ArtifactError::Integrity)?).bind(chunks).bind(initial.organization).bind(initial.acl).bind(&r.region_id)
            .bind(f64::from(r.ttl_seconds)).execute(&mut *tx).await?;
        let row = lookup(&mut tx, owner, &r.grant_id, None)
            .await?
            .ok_or(ArtifactError::Conflict)?;
        if row.try_get::<String, _>("request_digest")? != key
            || row.try_get::<String, _>("author_id")? != actor.user
        {
            return Err(ArtifactError::Conflict);
        }
        let accepted = receipt(&row)?;
        authority::finish(tx, actor).await?;
        Ok(accepted)
    }

    #[cfg(unix)]
    async fn grant_source(
        &self,
        tx: &mut Tx<'_>,
        actor: &ArtifactActor,
        owner: &str,
        r: &CreateTranscriptionInputGrant,
    ) -> Result<ReadStamp, ArtifactError> {
        authority::scope(tx, actor, owner, true).await?;
        crate::store::check_budget_actor(tx, &actor.user, owner, &r.root_budget_id)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        sqlx::query("SELECT root_budget_id FROM budget_roots WHERE root_budget_id=$1 AND owner_id=$2 AND state='active' FOR SHARE")
            .bind(&r.root_budget_id).bind(owner).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let row = self.row_in(tx, owner, &r.upload_id).await?;
        authority::resource(tx, actor, owner, &row, false, false).await?;
        authority::resource(tx, actor, owner, &row, true, false).await?;
        let v = view(&row)?;
        if v.state != "available"
            || v.version_id != r.measurement.version_id
            || v.artifact_id != r.measurement.artifact_id
            || v.sha256 != r.measurement.audio_sha256
        {
            return Err(ArtifactError::Conflict);
        }
        authority::stamp(tx, &row).await
    }

    pub(super) async fn revoke_audio_input(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        id: &str,
    ) -> Result<TranscriptionInputWithdrawal, ArtifactError> {
        if !valid_id(owner) || !valid_id(id) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        // An issuer can withdraw consent after losing the organization's content ACL.
        let personal: String =
            sqlx::query_scalar("SELECT personal_owner_id FROM users WHERE user_id=$1")
                .bind(&actor.user)
                .fetch_one(&mut *tx)
                .await?;
        authority::scope(&mut tx, actor, &personal, true).await?;
        consent_lock(&mut tx, owner, id).await?;
        let issuer:Option<String>=sqlx::query_scalar("SELECT author_id FROM transcription_input_grants WHERE owner_id=$1 AND grant_id=$2 FOR UPDATE")
            .bind(owner).bind(id).fetch_optional(&mut *tx).await?;
        if let Some(issuer) = issuer {
            if issuer != actor.user {
                return Err(ArtifactError::NotFound);
            }
        } else {
            authority::scope(&mut tx, actor, owner, true).await?;
        }
        sqlx::query("INSERT INTO transcription_input_revocations(owner_id,grant_id,revoked_by) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(owner).bind(id).bind(&actor.user).execute(&mut *tx).await?;
        authority::finish(tx, actor).await?;
        Ok(TranscriptionInputWithdrawal {
            grant_id: id.to_owned(),
            state: "revoked".into(),
        })
    }
}
