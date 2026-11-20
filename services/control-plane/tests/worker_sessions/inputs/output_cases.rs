//! Actual PG/mTLS/bytes/commit/read authorization. Candidate, catalog, prices,
//! enrollment and initial OS/device admission are explicit fixtures, not inference.
use super::*;
use orange_protocol::model::ResponseCandidate;
fn candidate() -> ResponseCandidate {
    ResponseCandidate {
        response_id: "output-fixture".into(),
        text: "actual stored output bytes ".repeat(1800),
        tool_calls: vec![],
        native_usage: None,
        finish_reason: "stop".into(),
    }
}
async fn begin(
    f: &InputFixture,
    server: &Running,
) -> (ControlConnection, worker::OutputHeader, Vec<u8>) {
    let (mut c, mut n, mut j, record) = receiver_capabilities(f, server, true, true).await;
    let (mut broker, supervisor) = broker(f).await;
    let grant = c
        .reserve_execution(&mut j, &mut broker, &supervisor, "cpu", &record.command_id)
        .expect("reserve fixture");
    c.authorize_execution(&mut j, &mut n, "output-fixture", &record.command_id)
        .await
        .expect("cloud start")
        .confirm(&mut j, &mut broker, &supervisor)
        .expect("confirm");
    let a = assignment(f).await;
    let candidate = candidate();
    let bytes = serde_json::to_vec(&candidate).expect("JSON");
    let header = worker::OutputHeader {
        transcription_output_version: 0,
        session_id: c.identity().session_id.clone(),
        upload_id: Uuid::new_v4().to_string(),
        accepted: Some(worker::Acknowledge {
            command_id: record.command_id,
            payload_digest: a.payload_digest,
            lease: a.lease,
            result: worker::AckResult::Accepted as i32,
            execution_journal_id: record.journal_id,
            broker_grant_id: grant.grant_id,
            broker_epoch: grant.broker_epoch as u64,
            slot_generation: grant.slot_generation as u64,
        }),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
        response_digest: request_digest("worker.cpu.candidate.v1", &candidate).expect("digest"),
    };
    (c, header, bytes)
}
fn frames(h: &worker::OutputHeader, bytes: &[u8]) -> Vec<worker::OutputFrame> {
    let mut frames = vec![worker::OutputFrame {
        body: Some(worker::output_frame::Body::Header(h.clone())),
    }];
    for (i, data) in bytes.chunks(32768).enumerate() {
        frames.push(worker::OutputFrame {
            body: Some(worker::output_frame::Body::Data(worker::OutputData {
                offset: (i * 32768) as u64,
                data: data.to_vec(),
            })),
        });
    }
    frames
}
async fn commits(f: &InputFixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM job_response_commits WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("commits")
}
#[tokio::test]
async fn exact_output_bytes_commit_once_are_user_readable_and_never_settle_money() {
    let f = setup("client-bv").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT row_to_json(w) FROM wallets w WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("wallet");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let receipt = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("actual write and commit")
        .into_inner();
    assert_eq!(receipt.upload_id, h.upload_id);
    assert_eq!(receipt.response_digest, h.response_digest);
    let again = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("same immutable bytes")
        .into_inner();
    assert_eq!(receipt, again);
    assert_eq!(commits(&f).await, 1);
    let view = f
        .objects
        .job_result(&f.actor, &f.owner, &f.job)
        .await
        .expect("current user result");
    assert_eq!(
        view.version_id,
        receipt.artifact.as_ref().expect("artifact").version_id
    );
    let (_, stored, _) = f
        .objects
        .read(&f.actor, &f.owner, &view.upload_id, &view.version_id)
        .await
        .expect("actual output bytes");
    assert_eq!(stored.to_vec(), bytes);
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut request = axum::http::Request::builder()
        .uri(format!("/api/v1/owners/{}/jobs/{}/result", f.owner, f.job))
        .body(axum::body::Body::empty())
        .expect("request");
    *request.headers_mut() = f.headers.clone();
    assert_eq!(app.oneshot(request).await.expect("HTTP").status(), 200);
    let job = f.d.store.job(&f.owner, &f.job).await.expect("job");
    assert_eq!(job.execution_state, "succeeded");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT row_to_json(w) FROM wallets w WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("wallet");
    assert_eq!(before, after);
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.entity_id=$1 AND event_type='job.response_committed'").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("event count");
    assert_eq!(events, 1);
    let input = tokio_stream::iter(vec![
        Ok::<_, orange_control_plane::artifacts::ArtifactError>(opendal::Buffer::from(bytes)),
    ]);
    assert!(
        f.objects
            .put(f.actor.clone(), f.owner.clone(), h.upload_id.clone(), input)
            .await
            .is_err(),
        "user content route cannot replace Worker output"
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn malformed_chunk_is_invisible_and_exact_retry_preserves_binding() {
    let f = setup("client-bw").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let mut broken = frames(&h, &bytes);
    if let Some(worker::output_frame::Body::Data(data)) = &mut broken[1].body {
        data.offset = 1;
    }
    assert!(raw.commit_output(tokio_stream::iter(broken)).await.is_err());
    assert_eq!(commits(&f).await, 0);
    assert!(
        f.objects
            .job_result(&f.actor, &f.owner, &f.job)
            .await
            .is_err()
    );
    let receipt = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("explicit byte-identical transfer retry")
        .into_inner();
    assert_eq!(commits(&f).await, 1);
    assert_eq!(receipt.upload_id, h.upload_id);
    let orphan:i64=sqlx::query_scalar("SELECT count(*) FROM artifact_transfers t JOIN worker_output_uploads w USING(artifact_id) WHERE w.job_id=$1 AND t.state='orphan' AND t.reserved_bytes>0").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("orphan accounting");
    assert_eq!(orphan, 1);
    let mut changed = h.clone();
    changed.response_digest = "a".repeat(64);
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&changed, &bytes)))
            .await
            .is_err()
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn source_deletion_fences_output_reads_replay_and_recovery() {
    let f = setup("client-bx").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let r = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("commit")
        .into_inner();
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("delete original source");
    assert!(
        f.objects
            .job_result(&f.actor, &f.owner, &f.job)
            .await
            .is_err()
    );
    assert!(
        f.objects
            .read(
                &f.actor,
                &f.owner,
                &h.upload_id,
                &r.artifact.expect("artifact").version_id
            )
            .await
            .is_err()
    );
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err()
    );
    assert!(
        raw.query_output(worker::OutputQuery {
            session_id: c.identity().session_id.clone(),
            original: Some(h)
        })
        .await
        .is_err()
    );
    assert_eq!(commits(&f).await, 1);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn output_query_recovers_original_fact_under_new_session_without_new_effect() {
    let f = setup("client-by").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let r = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("original commit")
        .into_inner();
    let mut greeting = hello(&f.d.node);
    if let Some(worker_frame::Body::Hello(h)) = &mut greeting.body {
        h.node_control_version = 2;
        h.execution_control_version = 2;
        h.rejection_receipts_version = 1;
        h.input_artifacts_version = 1;
        h.renewal_version = 1;
        h.output_version = 1;
    }
    let (sender, mut stream) = control(&mut raw, greeting).await.expect("new session");
    let w = welcome(&mut stream).await;
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err(),
        "old execution cannot publish under new epoch"
    );
    let found = raw
        .query_output(worker::OutputQuery {
            session_id: w.session_id,
            original: Some(h),
        })
        .await
        .expect("historical exact receipt")
        .into_inner();
    assert_eq!(found, r);
    assert_eq!(commits(&f).await, 1);
    drop(sender);
    drop(stream);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn committed_result_reads_use_current_content_rights_not_submission_permission() {
    let f = setup_model_scope(
        "client-cb",
        &"b".repeat(64),
        &"c".repeat(64),
        "organization fixture".into(),
        32,
        true,
    )
    .await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("commit");
    sqlx::query("UPDATE memberships SET role='viewer' WHERE organization_id=$1 AND user_id=$2")
        .bind(&f.owner)
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("read-only role fixture");
    sqlx::query("UPDATE budget_user_grants SET can_submit=false WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("cannot spend again");
    sqlx::query(
        "UPDATE organizations SET policy_version=policy_version+1 WHERE organization_id=$1",
    )
    .bind(&f.owner)
    .execute(f.d.store.pool())
    .await
    .expect("policy revision");
    let result = f
        .objects
        .job_result(&f.actor, &f.owner, &f.job)
        .await
        .expect("viewer can read already committed result with both ACLs");
    f.objects
        .read(&f.actor, &f.owner, &result.upload_id, &result.version_id)
        .await
        .expect("read rights independent of submitting another job");
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err()
    );
    raw.query_output(worker::OutputQuery {
        session_id: c.identity().session_id.clone(),
        original: Some(h.clone()),
    })
    .await
    .expect("historical read fact only");
    sqlx::query("DELETE FROM resource_acl WHERE resource_id=(SELECT artifact_id FROM job_inputs WHERE job_id=$1) AND user_id=$2").bind(&f.job).bind(&f.user).execute(f.d.store.pool()).await.expect("source ACL revoked");
    assert!(
        f.objects
            .job_result(&f.actor, &f.owner, &f.job)
            .await
            .is_err()
    );
    assert!(
        raw.query_output(worker::OutputQuery {
            session_id: c.identity().session_id.clone(),
            original: Some(h)
        })
        .await
        .is_err()
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn corrupted_published_output_cannot_be_read_or_recovered_as_valid() {
    let f = setup("client-cc").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let receipt = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await
        .expect("commit")
        .into_inner();
    let key:String=sqlx::query_scalar("SELECT t.published_key FROM artifacts a JOIN artifact_transfers t ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1").bind(&receipt.artifact.as_ref().expect("artifact").artifact_id).fetch_one(f.d.store.pool()).await.expect("exact owned test blob");
    let mut corrupt = bytes.clone();
    corrupt[0] = b'!';
    std::fs::write(f.root.join(key), corrupt).expect("explicit same-size storage fault");
    assert!(
        f.objects
            .read(
                &f.actor,
                &f.owner,
                &h.upload_id,
                &receipt.artifact.expect("artifact").version_id
            )
            .await
            .is_err()
    );
    assert!(
        raw.query_output(worker::OutputQuery {
            session_id: c.identity().session_id.clone(),
            original: Some(h.clone())
        })
        .await
        .is_err()
    );
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err()
    );
    assert_eq!(commits(&f).await, 1);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn source_deleted_during_upload_never_becomes_a_committed_output() {
    let f = setup("client-cd").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let mut parts = frames(&h, &bytes).into_iter();
    sender
        .send(parts.next().expect("header"))
        .await
        .expect("header send");
    let upload =
        tokio::spawn(async move { raw.commit_output(ReceiverStream::new(receiver)).await });
    sender
        .send(parts.next().expect("first bytes"))
        .await
        .expect("chunk");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let started: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM worker_output_uploads WHERE job_id=$1)",
            )
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("started");
            if started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned transfer started");
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("source delete");
    for part in parts {
        if sender.send(part).await.is_err() {
            break;
        }
    }
    drop(sender);
    assert!(upload.await.expect("task").is_err());
    assert_eq!(commits(&f).await, 0);
    assert!(
        f.objects
            .job_result(&f.actor, &f.owner, &f.job)
            .await
            .is_err()
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn concurrent_identical_uploads_have_one_commit_and_one_stored_transfer() {
    let f = setup("client-cf").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut first = server.client(Some(&f.d.certificate)).await.expect("first");
    let mut second = first.clone();
    let (a, b) = tokio::join!(
        first.commit_output(tokio_stream::iter(frames(&h, &bytes))),
        second.commit_output(tokio_stream::iter(frames(&h, &bytes)))
    );
    assert!(a.is_ok() || b.is_ok());
    if let (Ok(a), Ok(b)) = (a, b) {
        assert_eq!(a.into_inner(), b.into_inner());
    }
    assert_eq!(commits(&f).await, 1);
    let transfers:i64=sqlx::query_scalar("SELECT count(*) FROM artifact_transfers t JOIN worker_output_uploads w USING(artifact_id) WHERE w.job_id=$1").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("one transfer");
    assert_eq!(transfers, 1);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn readonly_gateway_cannot_silently_offer_output_writer() {
    let f = setup("client-cg").await;
    bind(&f).await;
    let server = Running::start_with_input(&f.d.store, Some(f.objects.reader())).await;
    let root = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("root"));
    let mut node = NodeJournal::open(
        &root.join(format!("node-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("node");
    let mut execution = ExecutionJournal::open(
        &root.join(format!("execution-{}.sqlite", Uuid::new_v4())),
        f.d.node.clone(),
    )
    .expect("execution");
    assert!(
        ControlConnection::connect_output_execution(
            worker_config(&server, &f.d),
            &mut node,
            &mut execution
        )
        .await
        .is_err()
    );
    assert_eq!(commits(&f).await, 0);
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn response_commit_wait_crossing_lease_deadline_never_publishes() {
    let f = setup("client-ch").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let job = Uuid::parse_str(&f.job).expect("generated ID");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION output_delay_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.job_id='{job}' THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE TRIGGER output_delay_{suffix} BEFORE INSERT ON job_response_commits FOR EACH ROW EXECUTE FUNCTION output_delay_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("isolated real DB delay");
    sqlx::query("UPDATE worker_execution_attempts SET ack_deadline=clock_timestamp()-interval '1 second',lease_expires_at=clock_timestamp()+interval '2 seconds' WHERE job_id=$1").bind(&f.job).execute(f.d.store.pool()).await.expect("short live fixture lease");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let result = raw
        .commit_output(tokio_stream::iter(frames(&h, &bytes)))
        .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER output_delay_{suffix} ON job_response_commits; DROP FUNCTION output_delay_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact fault");
    assert!(result.is_err());
    assert_eq!(commits(&f).await, 0);
    assert!(
        f.objects
            .job_result(&f.actor, &f.owner, &f.job)
            .await
            .is_err()
    );
    assert_eq!(
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("job")
            .execution_state,
        "running"
    );
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn unrequested_model_tool_call_never_becomes_a_committed_response() {
    let f = setup("client-ci").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, mut h, _) = begin(&f, &server).await;
    let mut response = candidate();
    response.finish_reason = "tool_calls".into();
    response.tool_calls = vec![orange_protocol::model::ToolCall {
        id: "call".into(),
        name: "not_requested".into(),
        arguments: serde_json::json!({"path":"untrusted"}),
    }];
    let bytes = serde_json::to_vec(&response).expect("JSON");
    h.sha256 = hex::encode(Sha256::digest(&bytes));
    h.size_bytes = bytes.len() as u64;
    h.response_digest = request_digest("worker.cpu.candidate.v1", &response).expect("digest");
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err()
    );
    assert_eq!(commits(&f).await, 0);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn duplicate_json_response_keys_are_not_canonicalized_into_success() {
    let f = setup("client-cj").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, mut h, bytes) = begin(&f, &server).await;
    let text = String::from_utf8(bytes).expect("UTF8").replacen(
        "\"response_id\":\"output-fixture\"",
        "\"response_id\":\"output-fixture\",\"response_id\":\"output-fixture\"",
        1,
    );
    let bytes = text.into_bytes();
    h.sha256 = hex::encode(Sha256::digest(&bytes));
    h.size_bytes = bytes.len() as u64;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    assert!(
        raw.commit_output(tokio_stream::iter(frames(&h, &bytes)))
            .await
            .is_err()
    );
    assert_eq!(commits(&f).await, 0);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn database_refuses_cross_artifact_version_in_response_commit() {
    let f = setup("client-ck").await;
    bind(&f).await;
    let server = Running::start_with_output(&f.d.store, f.objects.clone()).await;
    let (c, h, bytes) = begin(&f, &server).await;
    let mut raw = server.client(Some(&f.d.certificate)).await.expect("TLS");
    let mut broken = frames(&h, &bytes);
    if let Some(worker::output_frame::Body::Data(data)) = &mut broken[1].body {
        data.offset = 1;
    }
    assert!(raw.commit_output(tokio_stream::iter(broken)).await.is_err());
    let artifact: String =
        sqlx::query_scalar("SELECT artifact_id FROM worker_output_uploads WHERE job_id=$1")
            .bind(&f.job)
            .fetch_one(f.d.store.pool())
            .await
            .expect("staged output binding");
    let mut fault_tx =
        f.d.store
            .pool()
            .begin()
            .await
            .expect("isolated rollback-only fault");
    let wrong=sqlx::query("INSERT INTO job_response_commits(job_id,commit_id,artifact_id,version_id,response_digest,receipt) VALUES($1,$2,$3,$4,$5,$6)").bind(&f.job).bind(Uuid::new_v4().to_string()).bind(artifact).bind(&f.version).bind(&h.response_digest).bind(Vec::<u8>::new()).execute(&mut *fault_tx).await;
    fault_tx
        .rollback()
        .await
        .expect("always roll back the injected attempt");
    assert!(
        wrong.is_err(),
        "valid source version cannot be substituted for output version"
    );
    assert_eq!(
        wrong
            .as_ref()
            .err()
            .and_then(sqlx::Error::as_database_error)
            .and_then(|e| e.constraint()),
        Some("response_exact_artifact_version")
    );
    assert_eq!(commits(&f).await, 0);
    c.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
