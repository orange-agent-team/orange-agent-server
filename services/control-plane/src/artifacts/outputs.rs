//! Lease-scoped Worker output. No fabricated User session and no billing effect.
use super::*;
use crate::worker::{DeviceSession, WorkerSessionStore};
use orange_protocol::{
    execution::{acknowledge_digest, output_header_digest},
    grpc::worker,
};
use prost::Message;
const MEDIA: &str = "application/vnd.orange.model-response+json";
struct OutputScope {
    job: String,
    owner: String,
    user: String,
    organization: bool,
    header_digest: String,
    input: worker::InputArtifact,
    transcription: Option<super::transcription_output::TranscriptSource>,
}
impl ArtifactStore {
    async fn output_resource(
        &self,
        tx: &mut Tx<'_>,
        scope: &OutputScope,
        row: &sqlx::postgres::PgRow,
    ) -> Result<(), ArtifactError> {
        let role = authority::user_scope(tx, &scope.user, &scope.owner, true).await?;
        match (role, row.try_get::<Option<String>, _>("resource_id")?) {
            (None, None) => Ok(()),
            (Some(role), Some(resource)) => {
                sqlx::query("SELECT resource_id FROM team_resources WHERE resource_id=$1 AND organization_id=$2 AND kind='artifact' AND NOT deleted FOR SHARE").bind(&resource).bind(&scope.owner).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
                crate::team::resource_permission(
                    tx,
                    &scope.user,
                    &resource,
                    role,
                    orange_protocol::team::ResourceAccess::Write,
                )
                .await
                .map_err(|_| ArtifactError::Forbidden)
            }
            _ => Err(ArtifactError::Forbidden),
        }
    }
    async fn output_scope(
        &self,
        tx: &mut Tx<'_>,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        historical: bool,
    ) -> Result<OutputScope, ArtifactError> {
        sqlx::query("SET LOCAL lock_timeout = '3s'")
            .execute(&mut **tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '4s'")
            .execute(&mut **tx)
            .await?;
        if s.output_version != 1 {
            return Err(ArtifactError::Forbidden);
        }
        sessions
            .authorize_in(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        let ack = h.accepted.as_ref().ok_or(ArtifactError::Invalid)?;
        let lease = ack.lease.as_ref().ok_or(ArtifactError::Invalid)?;
        let attempt=sqlx::query("SELECT *,lease_expires_at>clock_timestamp() AS live FROM worker_execution_attempts WHERE command_id=$1 AND node_id=$2").bind(&ack.command_id).bind(&s.node_id).fetch_optional(&mut **tx).await?.ok_or(ArtifactError::Forbidden)?;
        let assignment =
            crate::worker::execution::decode(&attempt).map_err(|_| ArtifactError::Integrity)?;
        let digest = output_header_digest(h, &assignment).ok_or(ArtifactError::Invalid)?;
        if attempt.try_get::<Option<String>, _>("ack_digest")?
            != acknowledge_digest(ack, &assignment)
        {
            return Err(ArtifactError::Forbidden);
        }
        if !historical {
            let (state, generation): (String, i64) =
                sqlx::query_as("SELECT state,policy_generation FROM nodes WHERE node_id=$1")
                    .bind(&s.node_id)
                    .fetch_one(&mut **tx)
                    .await?;
            if s.session_id != h.session_id
                || lease.connection_epoch != s.connection_epoch as u64
                || state != "online"
                || assignment.node_policy_generation != generation as u64
                || !attempt.try_get::<bool, _>("live")?
                || attempt.try_get::<String, _>("state")? != "started"
            {
                return Err(ArtifactError::Forbidden);
            }
            let state = crate::worker::execution::output_job_authority(tx, &lease.job_id)
                .await
                .map_err(|_| ArtifactError::Forbidden)?;
            if !matches!(state.as_str(), "running" | "succeeded") {
                return Err(ArtifactError::Forbidden);
            }
            crate::worker::execution::eligible_plan(tx, &lease.job_id, &s.node_id)
                .await
                .map_err(|_| ArtifactError::Forbidden)?;
        }
        let row = sqlx::query("SELECT owner_id,submitting_user_id FROM jobs WHERE job_id=$1")
            .bind(&lease.job_id)
            .fetch_one(&mut **tx)
            .await?;
        let owner: String = row.try_get("owner_id")?;
        let user: String = row
            .try_get::<Option<String>, _>("submitting_user_id")?
            .ok_or(ArtifactError::Forbidden)?;
        let organization = authority::user_scope(tx, &user, &owner, !historical)
            .await?
            .is_some();
        let (input, transcription) = if assignment.transcription_input.is_some() {
            let (input, source) = self
                .transcript_source(tx, s, &assignment, &owner, &user, historical)
                .await?;
            let valid:bool=sqlx::query_scalar("SELECT j.reserved=0 OR EXISTS(SELECT 1 FROM transcription_quote_terms t WHERE t.quote_id=j.quote_id
                AND t.requester_id=j.submitting_user_id AND t.grant_id=$2) FROM jobs j WHERE j.job_id=$1")
                .bind(&lease.job_id).bind(&source.grant_id).fetch_one(&mut **tx).await?;
            if !valid {
                return Err(ArtifactError::PricingUnavailable);
            }
            (input, Some(source))
        } else {
            let input = if historical {
                inputs::source_read_in(tx, &lease.job_id, &user).await?
            } else {
                inputs::check_input_in(tx, &lease.job_id)
                    .await?
                    .ok_or(ArtifactError::Forbidden)?
            };
            if assignment.input_artifacts != vec![input.clone()] {
                return Err(ArtifactError::Integrity);
            }
            (input, None)
        };
        Ok(OutputScope {
            job: lease.job_id.clone(),
            owner,
            user,
            organization,
            header_digest: digest,
            input,
            transcription,
        })
    }
    async fn finish_output_scope(
        &self,
        mut tx: Tx<'_>,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        historical: bool,
    ) -> Result<(), ArtifactError> {
        if !historical {
            let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_execution_attempts WHERE command_id=$1 AND session_id=$2 AND state='started' AND lease_expires_at>clock_timestamp())").bind(&h.accepted.as_ref().ok_or(ArtifactError::Invalid)?.command_id).bind(&s.session_id).fetch_one(&mut *tx).await?;
            if !live {
                return Err(ArtifactError::Expired);
            }
        }
        if !historical && h.transcription_output_version == 1 {
            let lease = h
                .accepted
                .as_ref()
                .and_then(|a| a.lease.as_ref())
                .ok_or(ArtifactError::Invalid)?;
            let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs j JOIN quotes q USING(quote_id) JOIN quote_execution_plans p USING(quote_id)
                JOIN transcription_input_candidates c ON c.owner_id=j.owner_id AND c.invocation_id=j.invocation_id AND c.author_id=j.submitting_user_id
                AND c.root_budget_id=j.root_budget_id AND c.projection_digest=q.projection_digest AND c.node_id=p.target_node_id AND c.execution_profile_id=p.execution_profile_id
                WHERE j.job_id=$1 AND c.session_id=$2 AND c.connection_epoch=$3 AND p.job_deadline>clock_timestamp())")
                .bind(&lease.job_id).bind(&s.session_id).bind(s.connection_epoch).fetch_one(&mut *tx).await?;
            if !live {
                return Err(ArtifactError::Forbidden);
            }
        }
        crate::worker::sessions::finish(tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)
    }
    async fn prepare_output(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
    ) -> Result<Transfer, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '3s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '4s'")
            .execute(&mut *tx)
            .await?;
        sessions
            .authorize_in(&mut tx, s)
            .await
            .map_err(|_| ArtifactError::Forbidden)?;
        // Capacity-before-artifact agrees with source deletion/maintenance;
        // don't hold a source SHARE lock while waiting on its deletion's cap.
        let cap = sqlx::query("SELECT * FROM artifact_capacity WHERE root_id=$1 FOR UPDATE")
            .bind(&self.inner.objects.root_id)
            .fetch_one(&mut *tx)
            .await?;
        let scope = self.output_scope(&mut tx, sessions, s, h, false).await?;
        let existing = sqlx::query("SELECT * FROM worker_output_uploads WHERE job_id=$1")
            .bind(&scope.job)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(existing) = existing {
            if existing.try_get::<String, _>("header_digest")? != scope.header_digest {
                return Err(ArtifactError::Conflict);
            }
        } else {
            let state: String =
                sqlx::query_scalar("SELECT execution_state FROM jobs WHERE job_id=$1")
                    .bind(&scope.job)
                    .fetch_one(&mut *tx)
                    .await?;
            if state != "running" {
                return Err(ArtifactError::Conflict);
            }
            if cap.try_get::<i64, _>("records")? >= cap.try_get::<i64, _>("record_limit")? {
                return Err(ArtifactError::Capacity);
            }
            let conflict: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM artifact_uploads WHERE owner_id=$1 AND upload_id=$2)",
            )
            .bind(&scope.owner)
            .bind(&h.upload_id)
            .fetch_one(&mut *tx)
            .await?;
            if conflict {
                return Err(ArtifactError::Conflict);
            }
            let artifact = id();
            let version = id();
            if scope.organization {
                sqlx::query("INSERT INTO team_resources(resource_id,organization_id,creator_id,kind) VALUES($1,$2,$3,'artifact')").bind(&artifact).bind(&scope.owner).bind(&scope.user).execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO resource_acl(resource_id,user_id,access) VALUES($1,$2,'write')",
                )
                .bind(&artifact)
                .bind(&scope.user)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("INSERT INTO artifacts(artifact_id,version_id,root_id,owner_id,creator_id,resource_id,sha256,size_bytes,media_type) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(&artifact).bind(&version).bind(&self.inner.objects.root_id).bind(&scope.owner).bind(&scope.user).bind(scope.organization.then_some(&artifact)).bind(&h.sha256).bind(h.size_bytes as i64).bind(if scope.transcription.is_some(){orange_protocol::transcript::MEDIA}else{MEDIA}).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO artifact_uploads(owner_id,upload_id,artifact_id,request_digest) VALUES($1,$2,$3,$4)").bind(&scope.owner).bind(&h.upload_id).bind(&artifact).bind(&scope.header_digest).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO worker_output_uploads(job_id,attempt_id,node_id,upload_id,artifact_id,header_digest,response_digest,original_header) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(&scope.job).bind(&h.accepted.as_ref().and_then(|a|a.lease.as_ref()).ok_or(ArtifactError::Invalid)?.attempt_id).bind(&s.node_id).bind(&h.upload_id).bind(&artifact).bind(&scope.header_digest).bind(&h.response_digest).bind(h.encode_to_vec()).execute(&mut *tx).await?;
            sqlx::query("UPDATE artifact_capacity SET records=records+1 WHERE root_id=$1")
                .bind(&self.inner.objects.root_id)
                .execute(&mut *tx)
                .await?;
        }
        let row = self.row_in(&mut tx, &scope.owner, &h.upload_id).await?;
        self.output_resource(&mut tx, &scope, &row).await?;
        let v = view(&row)?;
        let stamp = authority::stamp(&mut tx, &row).await?;
        if v.state == "deleted" {
            return Err(ArtifactError::Deleted);
        }
        if v.state == "available" {
            let key:String=sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE transfer_id=$1 AND state='attached' AND NOT published_deleted").bind(row.try_get::<Option<String>,_>("published_transfer_id")?).fetch_one(&mut *tx).await?;
            self.finish_output_scope(tx, s, h, false).await?;
            return Ok(Transfer {
                id: String::new(),
                stage: String::new(),
                published: key,
                view: v,
                replay: true,
                stamp,
            });
        }
        if !row.try_get::<bool, _>("upload_live")? {
            return Err(ArtifactError::Expired);
        }
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifact_transfers WHERE artifact_id=$1 AND state='writing')").bind(&v.artifact_id).fetch_one(&mut *tx).await?;
        if busy {
            return Err(ArtifactError::Busy);
        }
        let reserved = (h.size_bytes as i64) * 2;
        if cap
            .try_get::<i64, _>("used_bytes")?
            .checked_add(reserved)
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
        sqlx::query("INSERT INTO artifact_transfers(transfer_id,artifact_id,stage_key,published_key,reserved_bytes,state,expires_at) VALUES($1,$2,$3,$4,$5,'writing',clock_timestamp()+interval '30 seconds')").bind(&transfer.id).bind(&transfer.view.artifact_id).bind(&transfer.stage).bind(&transfer.published).bind(reserved).execute(&mut *tx).await?;
        sqlx::query("UPDATE artifact_capacity SET used_bytes=used_bytes+$2 WHERE root_id=$1")
            .bind(&self.inner.objects.root_id)
            .bind(reserved)
            .execute(&mut *tx)
            .await?;
        self.finish_output_scope(tx, s, h, false).await?;
        Ok(transfer)
    }
    pub(crate) async fn commit_output<S>(
        &self,
        sessions: WorkerSessionStore,
        s: DeviceSession,
        h: worker::OutputHeader,
        input: S,
    ) -> Result<worker::OutputReceipt, ArtifactError>
    where
        S: Stream<Item = Result<worker::OutputData, ArtifactError>> + Send + Unpin + 'static,
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
            let result = store.write_output(&sessions, &s, &h, input).await;
            let _ = send.send(result);
        })
        .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }
    async fn write_output<S>(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        mut input: S,
    ) -> Result<worker::OutputReceipt, ArtifactError>
    where
        S: Stream<Item = Result<worker::OutputData, ArtifactError>> + Send + Unpin,
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        let _slot = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.inner.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ArtifactError::Expired)?
        .map_err(|_| ArtifactError::Unavailable)?;
        let t = self.prepare_output(sessions, s, h).await?;
        let mut stage: Option<opendal::Writer> = None;
        let mut published: Option<opendal::Writer> = None;
        let result = async {
            if !t.replay {
                stage = Some(self.inner.objects.writer(&t.stage).await?);
            }
            let mut bytes = Vec::with_capacity(h.size_bytes as usize);
            let mut frames = 0;
            while let Some(chunk) =
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), input.next())
                    .await
                    .map_err(|_| ArtifactError::Expired)?
            {
                let chunk = chunk?;
                frames += 1;
                if frames > 64
                    || chunk.offset != bytes.len() as u64
                    || chunk.data.is_empty()
                    || chunk.data.len() > 32768
                    || bytes.len() + chunk.data.len() > h.size_bytes as usize
                {
                    return Err(ArtifactError::Integrity);
                }
                let mut tx = self.inner.pool.begin().await?;
                self.output_scope(&mut tx, sessions, s, h, false).await?;
                self.finish_output_scope(tx, s, h, false).await?;
                bytes.extend_from_slice(&chunk.data);
                if let Some(writer) = &mut stage {
                    writer.write(chunk.data).await?;
                }
            }
            if bytes.len() as u64 != h.size_bytes || hex::encode(Sha256::digest(&bytes)) != h.sha256
            {
                return Err(ArtifactError::Integrity);
            }
            let usage = match self.validate_response(sessions, s, h, &bytes).await {
                Ok(usage) => usage,
                Err(error @ (ArtifactError::Invalid | ArtifactError::Integrity)) => {
                    // Only the server's job can supply an owner. Inspect field presence without retaining
                    // caller-provided identities or allocating another copy of the candidate content.
                    #[derive(serde::Deserialize)]
                    struct DeclaredOwner {
                        owner_id: Option<serde::de::IgnoredAny>,
                        owner: Option<serde::de::IgnoredAny>,
                    }
                    let reason = if matches!(error, ArtifactError::Integrity) {
                        "evidence_integrity"
                    } else if serde_json::from_slice::<DeclaredOwner>(&bytes)
                        .is_ok_and(|v| v.owner_id.is_some() || v.owner.is_some())
                    {
                        "untrusted_owner"
                    } else {
                        "invalid_candidate"
                    };
                    self.record_metering_rejection(sessions, s, h, reason)
                        .await?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            if let Some(mut writer) = stage.take() {
                writer.close().await?;
                self.inner.objects.finish_write(&t.stage)?;
            }
            if !t.replay {
                let stored = self.inner.objects.read(&t.stage, h.size_bytes).await?;
                if hex::encode(Sha256::digest(stored.to_vec())) != h.sha256 {
                    return Err(ArtifactError::Integrity);
                }
                published = Some(self.inner.objects.writer(&t.published).await?);
                published
                    .as_mut()
                    .ok_or(ArtifactError::Unavailable)?
                    .write(stored)
                    .await?;
                published
                    .take()
                    .ok_or(ArtifactError::Unavailable)?
                    .close()
                    .await?;
                self.inner.objects.finish_write(&t.published)?;
            }
            let actual = self.inner.objects.read(&t.published, h.size_bytes).await?;
            if hex::encode(Sha256::digest(actual.to_vec())) != h.sha256 {
                return Err(ArtifactError::Integrity);
            }
            if Instant::now() >= deadline {
                return Err(ArtifactError::Expired);
            }
            #[cfg(feature = "transcription-input-tests")]
            if h.transcription_output_version == 1
                && let Some((arrived, resume)) =
                    self.inner.transcript_publish_gate.lock().await.take()
            {
                let _ = arrived.send(());
                resume.await.map_err(|_| ArtifactError::Unavailable)?;
            }
            self.publish_output(sessions, s, h, &t, deadline, usage.as_ref())
                .await
        }
        .await;
        let mut close_error = None;
        for (writer, key) in [(stage.take(), &t.stage), (published.take(), &t.published)] {
            if let Some(mut writer) = writer {
                if let Err(e) = writer.close().await {
                    close_error = Some(ArtifactError::Storage(e));
                } else if let Err(e) = self.inner.objects.finish_write(key) {
                    close_error = Some(e);
                }
            }
        }
        if result.is_err() && !t.replay {
            // All owned writers have closed. A rejected private transcript can enter
            // physical collection immediately. An ambiguous or interrupted writer
            // still follows the existing fenced orphan-retention path.
            let state = if h.transcription_output_version == 1 && close_error.is_none() {
                "purging"
            } else {
                "orphan"
            };
            sqlx::query("UPDATE artifact_transfers SET state=$2,finished_at=clock_timestamp() WHERE transfer_id=$1 AND state='writing'")
                .bind(&t.id).bind(state).execute(&self.inner.pool).await?;
        }
        if let Some(e) = close_error {
            return Err(e);
        }
        result
    }
    async fn record_metering_rejection(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        reason: &str,
    ) -> Result<(), ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        let scope = self.output_scope(&mut tx, sessions, s, h, false).await?;
        let needed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_output_uploads u JOIN jobs j USING(job_id)
            WHERE u.upload_id=$1 AND j.job_id=$2 AND j.owner_id=$3 AND u.response_digest=$4 AND
            (EXISTS(SELECT 1 FROM quote_metering t WHERE t.quote_id=j.quote_id) OR EXISTS(SELECT 1 FROM transcription_quote_terms t WHERE t.quote_id=j.quote_id))
            AND NOT EXISTS(SELECT 1 FROM metering_input_rejections r WHERE r.upload_id=u.upload_id) FOR UPDATE OF u)")
            .bind(&h.upload_id).bind(&scope.job).bind(&scope.owner).bind(&h.response_digest).fetch_one(&mut *tx).await?;
        if needed {
            let case = uuid::Uuid::new_v4().to_string();
            let admitted = sqlx::query_scalar::<_, bool>(
                "SELECT NOT EXISTS(SELECT 1 FROM metering_input_rejections WHERE upload_id=$1)",
            )
            .bind(&h.upload_id)
            .fetch_one(&mut *tx)
            .await?;
            if admitted {
                sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,environment,business_ref,state,next_check_at)
                    VALUES($1,$2,'dev',$3,'open',floor(extract(epoch FROM clock_timestamp()))::bigint)")
                    .bind(&case).bind(&scope.owner).bind(format!("metering-input:{}",h.upload_id)).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO metering_input_rejections(upload_id,job_id,owner_id,response_digest,bytes_digest,reason,case_id) VALUES($1,$2,$3,$4,$5,$6,$7)")
                    .bind(&h.upload_id).bind(&scope.job).bind(&scope.owner).bind(&h.response_digest).bind(&h.sha256).bind(reason).bind(&case).execute(&mut *tx).await?;
            }
        }
        self.finish_output_scope(tx, s, h, false).await
    }
    async fn validate_response(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        bytes: &[u8],
    ) -> Result<Option<orange_model_runtime::meter_service::MeasuredOutput>, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        let scope = self.output_scope(&mut tx, sessions, s, h, false).await?;
        let key:String=sqlx::query_scalar("SELECT t.published_key FROM artifacts a JOIN artifact_transfers t ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1 AND a.root_id=$2 AND a.state='available' AND orange_artifact_content_live(a.artifact_id) AND t.state='attached' AND NOT t.published_deleted").bind(&scope.input.artifact_id).bind(&self.inner.objects.root_id).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::Integrity)?;
        self.finish_output_scope(tx, s, h, false).await?;
        let request = self
            .inner
            .objects
            .read(&key, scope.input.size_bytes)
            .await?
            .to_vec();
        if hex::encode(Sha256::digest(&request)) != scope.input.sha256 {
            return Err(ArtifactError::Integrity);
        }
        if let Some(audio) = scope.transcription {
            audio.validate(bytes, &h.response_digest)?;
            return Ok(None);
        }
        let value = orange_protocol::canonical::parse_tool_arguments(
            std::str::from_utf8(bytes).map_err(|_| ArtifactError::Invalid)?,
        )
        .map_err(|_| ArtifactError::Invalid)?;
        let candidate: orange_protocol::model::ResponseCandidate =
            serde_json::from_value(value).map_err(|_| ArtifactError::Invalid)?;
        if !valid_id(&candidate.response_id)
            || request_digest("worker.cpu.candidate.v1", &candidate)
                .map_err(|_| ArtifactError::Invalid)?
                != h.response_digest
        {
            return Err(ArtifactError::Integrity);
        }
        orange_model_runtime::metering::project_candidate(&candidate)
            .map_err(|_| ArtifactError::Invalid)?;
        let request: orange_protocol::model::GenerateRequest =
            serde_json::from_slice(&request).map_err(|_| ArtifactError::Integrity)?;
        request.validate().map_err(|_| ArtifactError::Integrity)?;
        if candidate
            .tool_calls
            .iter()
            .any(|c| !c.arguments.is_object() || !request.tools.iter().any(|t| t.name == c.name))
        {
            return Err(ArtifactError::Invalid);
        }
        let committed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM job_response_commits WHERE job_id=$1)")
                .bind(&scope.job)
                .fetch_one(&self.inner.pool)
                .await?;
        if committed {
            Ok(None)
        } else {
            self.measure_job_output(&scope.job, request, candidate)
                .await
        }
    }
    async fn publish_output(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
        t: &Transfer,
        deadline: Instant,
        usage: Option<&orange_model_runtime::meter_service::MeasuredOutput>,
    ) -> Result<worker::OutputReceipt, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        let scope = self.output_scope(&mut tx, sessions, s, h, false).await?;
        let row = self.row_in(&mut tx, &scope.owner, &h.upload_id).await?;
        self.output_resource(&mut tx, &scope, &row).await?;
        if authority::stamp(&mut tx, &row).await? != t.stamp
            || view(&row)? != t.view
            || Instant::now() >= deadline
        {
            return Err(ArtifactError::Conflict);
        }
        let receipt = if t.replay {
            let raw: Vec<u8> = sqlx::query_scalar(
                "SELECT receipt FROM job_response_commits WHERE job_id=$1 AND artifact_id=$2",
            )
            .bind(&scope.job)
            .bind(&t.view.artifact_id)
            .fetch_one(&mut *tx)
            .await?;
            worker::OutputReceipt::decode(raw.as_slice()).map_err(|_| ArtifactError::Integrity)?
        } else {
            let changed=sqlx::query("UPDATE artifact_transfers SET state='attached',finished_at=clock_timestamp() WHERE transfer_id=$1 AND state='writing' AND expires_at>clock_timestamp()").bind(&t.id).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(ArtifactError::Expired);
            }
            sqlx::query("UPDATE artifacts SET state='available',published_transfer_id=$2,published_at=clock_timestamp() WHERE artifact_id=$1 AND state='staging'").bind(&t.view.artifact_id).bind(&t.id).execute(&mut *tx).await?;
            let receipt = worker::OutputReceipt {
                commit_id: id(),
                job_id: scope.job.clone(),
                upload_id: h.upload_id.clone(),
                artifact: Some(worker::InputArtifact {
                    artifact_id: t.view.artifact_id.clone(),
                    version_id: t.view.version_id.clone(),
                    sha256: h.sha256.clone(),
                    size_bytes: h.size_bytes,
                    media_type: if scope.transcription.is_some() {
                        orange_protocol::transcript::MEDIA
                    } else {
                        MEDIA
                    }
                    .into(),
                }),
                response_digest: h.response_digest.clone(),
                header_digest: scope.header_digest,
            };
            sqlx::query("INSERT INTO job_response_commits(job_id,commit_id,artifact_id,version_id,response_digest,receipt) VALUES($1,$2,$3,$4,$5,$6)").bind(&scope.job).bind(&receipt.commit_id).bind(&t.view.artifact_id).bind(&t.view.version_id).bind(&h.response_digest).bind(receipt.encode_to_vec()).execute(&mut *tx).await?;
            if let Some(audio) = &scope.transcription {
                audio
                    .record(
                        &mut tx,
                        &scope.job,
                        &scope.owner,
                        &scope.input,
                        &h.response_digest,
                    )
                    .await?;
            }
            self.record_usage(&mut tx, &scope.job, &h.response_digest, usage)
                .await?;
            sqlx::query("INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,$2)")
                .bind(&t.view.artifact_id)
                .bind(format!("job-output:{}", scope.job))
                .execute(&mut *tx)
                .await?;
            let changed=sqlx::query("UPDATE jobs SET execution_state='succeeded' WHERE job_id=$1 AND execution_state='running' AND settlement_state='pending'").bind(&scope.job).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(ArtifactError::Conflict);
            }
            crate::events::append_attempt(
                &mut tx,
                &scope.owner,
                &scope.job,
                "job.response_committed",
                &h.response_digest,
                &h.accepted
                    .as_ref()
                    .and_then(|ack| ack.lease.as_ref())
                    .ok_or(ArtifactError::Invalid)?
                    .attempt_id,
            )
            .await?;
            crate::events::append(
                &mut tx,
                &scope.owner,
                &t.view.artifact_id,
                "artifact.available",
                &h.sha256,
            )
            .await?;
            receipt
        };
        self.finish_output_scope(tx, s, h, false).await?;
        // Publication is durable; a lost/late acknowledgement is recovered by
        // query, never by manufacturing another response or undoing history.
        if Instant::now() >= deadline {
            return Err(ArtifactError::Expired);
        }
        let mut check = self.inner.pool.begin().await?;
        self.output_scope(&mut check, sessions, s, h, false).await?;
        let row = self.row_in(&mut check, &scope.owner, &h.upload_id).await?;
        if row.try_get::<String, _>("state")? != "available" {
            return Err(ArtifactError::NotReady);
        }
        self.finish_output_scope(check, s, h, false).await?;
        Ok(receipt)
    }
    pub(crate) async fn query_output(
        &self,
        sessions: WorkerSessionStore,
        s: DeviceSession,
        h: worker::OutputHeader,
    ) -> Result<worker::OutputReceipt, ArtifactError> {
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
            let result = store.query_output_owned(&sessions, &s, &h).await;
            let _ = send.send(result);
        })
        .await?;
        receive.await.map_err(|_| ArtifactError::Unavailable)?
    }
    async fn query_output_owned(
        &self,
        sessions: &WorkerSessionStore,
        s: &DeviceSession,
        h: &worker::OutputHeader,
    ) -> Result<worker::OutputReceipt, ArtifactError> {
        let _slot = self
            .inner
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ArtifactError::Unavailable)?;
        let mut tx = self.inner.pool.begin().await?;
        let scope = self.output_scope(&mut tx, sessions, s, h, true).await?;
        let raw:Vec<u8>=sqlx::query_scalar("SELECT c.receipt FROM job_response_commits c JOIN worker_output_uploads w USING(job_id) WHERE c.job_id=$1 AND w.header_digest=$2").bind(&scope.job).bind(&scope.header_digest).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotReady)?;
        let receipt =
            worker::OutputReceipt::decode(raw.as_slice()).map_err(|_| ArtifactError::Integrity)?;
        let row = self.row_in(&mut tx, &scope.owner, &h.upload_id).await?;
        if row.try_get::<String, _>("state")? != "available" {
            return Err(ArtifactError::NotReady);
        }
        let key:String=sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE transfer_id=$1 AND state='attached' AND NOT published_deleted").bind(row.try_get::<Option<String>,_>("published_transfer_id")?).fetch_one(&mut *tx).await?;
        self.finish_output_scope(tx, s, h, true).await?;
        let bytes = self.inner.objects.read(&key, h.size_bytes).await?;
        if hex::encode(Sha256::digest(bytes.to_vec())) != h.sha256 {
            return Err(ArtifactError::Integrity);
        }
        let mut tx = self.inner.pool.begin().await?;
        self.output_scope(&mut tx, sessions, s, h, true).await?;
        let row = self.row_in(&mut tx, &scope.owner, &h.upload_id).await?;
        if row.try_get::<String, _>("state")? != "available" {
            return Err(ArtifactError::NotReady);
        }
        self.finish_output_scope(tx, s, h, true).await?;
        Ok(receipt)
    }
    pub async fn job_result(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        job: &str,
    ) -> Result<ArtifactView, ArtifactError> {
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let upload:Option<String>=sqlx::query_scalar("SELECT w.upload_id FROM job_response_commits c JOIN worker_output_uploads w USING(job_id) JOIN jobs j USING(job_id) WHERE c.job_id=$1 AND j.owner_id=$2").bind(job).bind(owner).fetch_optional(&mut *tx).await?;
        let row = self
            .row_in(&mut tx, owner, &upload.ok_or(ArtifactError::NotReady)?)
            .await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        let v = view(&row)?;
        if v.state != "available" {
            return Err(ArtifactError::NotReady);
        }
        authority::finish(tx, actor).await?;
        Ok(v)
    }
}
