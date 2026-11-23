//! Real fixed model counter + mTLS/PG/OpenDAL/development credits. Price/model
//! admission, identity, enrollment and submitted candidate are explicit fixtures.
use super::*;
use orange_model_runtime::meter_service::TrustedMeter;
use orange_protocol::pricing::{CreateTextQuote, TextQuoteView};
#[cfg(feature = "web-compute-tests")]
#[path = "browser_cases.rs"]
mod browser_cases;
#[path = "cancel_real.rs"]
mod cancel_real;
#[path = "cleanup_real.rs"]
mod cleanup_real;
struct Case {
    f: InputFixture,
    meter: TrustedMeter,
    _meter_root: tempfile::TempDir,
    price: String,
    budget: String,
}
async fn setup_priced(cert: &str) -> Case {
    let p = profile();
    let digest = cpu_profile_digest(&p).expect("digest");
    let root = runtime();
    let meter = TrustedMeter::launch(root.path(), profile())
        .await
        .expect("actual server-owned counter");
    let mut f = setup_model_meter(
        cert,
        &p.model_sha256,
        &digest,
        "Say a short greeting.".into(),
        32,
        false,
        Some(meter.clone()),
    )
    .await;
    f.d.store
        .cancel_queued(&f.owner, &f.job, "retire-unpriced-fixture")
        .await
        .expect("discard only the unpriced setup job");
    let budget: String = sqlx::query_scalar("SELECT root_budget_id FROM jobs WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("budget");
    f.request.invocation_id = Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec(&f.request).expect("input");
    f.upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    f.objects
        .begin_upload(&f.actor, &f.owner, &f.upload)
        .await
        .expect("new input intent");
    let input = f
        .objects
        .put(
            f.actor.clone(),
            f.owner.clone(),
            f.upload.upload_id.clone(),
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("actual input");
    f.version = input.version_id;
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1) ON CONFLICT(owner_id) DO NOTHING")
        .bind(&f.d.owner)
        .execute(f.d.store.pool())
        .await
        .expect("provider wallet fixture");
    let d = meter.descriptor();
    sqlx::query("INSERT INTO trusted_meter_profiles(profile_digest,descriptor) VALUES($1,$2) ON CONFLICT(profile_digest) DO NOTHING").bind(&d.profile_digest).bind(serde_json::to_value(d).expect("descriptor")).execute(f.d.store.pool()).await.expect("explicit supported profile admission fixture");
    let price = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO reviewed_cpu_profiles(execution_profile_id,executable_sha256,model_bytes,context_tokens,startup_timeout_ms) VALUES($1,$2,$3,$4,$5)").bind(&f.profile).bind(&p.executable_sha256).bind(p.model_bytes as i64).bind(p.context_tokens as i32).bind(p.startup_timeout.as_millis() as i64).execute(f.d.store.pool()).await.expect("explicit CPU profile admission fixture");
    sqlx::query("INSERT INTO text_prices(price_version,node_id,execution_profile_id,meter_profile_digest,input_price,output_price,output_limit,fee_bps) VALUES($1,$2,$3,$4,2000,6000,128,2000)").bind(&price).bind(&f.d.node).bind(&f.profile).bind(&d.profile_digest).execute(f.d.store.pool()).await.expect("development price admission fixture");
    Case {
        f,
        meter,
        _meter_root: root,
        price,
        budget,
    }
}
async fn idle(c: &Case, server: &Running) -> (ControlConnection, NodeJournal, ExecutionJournal) {
    let root = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("root"));
    let mut node = NodeJournal::open(
        &root.join(format!("node-{}.sqlite", Uuid::new_v4())),
        c.f.d.node.clone(),
    )
    .expect("node");
    let mut journal = ExecutionJournal::open(
        &root.join(format!("execution-{}.sqlite", Uuid::new_v4())),
        c.f.d.node.clone(),
    )
    .expect("journal");
    let connection = ControlConnection::connect_output_execution(
        worker_config(server, &c.f.d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("idle real control");
    (connection, node, journal)
}
fn quote_request(c: &Case, limit: u32) -> CreateTextQuote {
    CreateTextQuote {
        request_id: Uuid::new_v4().to_string(),
        root_budget_id: c.budget.clone(),
        price_version: c.price.clone(),
        input_upload_id: c.f.upload.upload_id.clone(),
        input_version_id: c.f.version.clone(),
        output_limit: limit,
    }
}
async fn quote_http(c: &Case, request: &CreateTextQuote) -> TextQuoteView {
    let app =
        orange_control_plane::artifacts::http::router(c.f.identity.clone(), c.f.objects.clone());
    let mut http = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/owners/{}/text-quotes", c.f.owner))
        .body(axum::body::Body::from(
            serde_json::to_vec(request).expect("JSON"),
        ))
        .expect("request");
    *http.headers_mut() = c.f.headers.clone();
    http.headers_mut()
        .insert("content-type", "application/json".parse().expect("header"));
    http.headers_mut()
        .insert("idempotency-key", request.request_id.parse().expect("key"));
    let response = app.oneshot(http).await.expect("actual quote route");
    assert_eq!(response.status(), 201);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("body"),
    )
    .expect("quote")
}
async fn submit(c: &mut Case, q: &TextQuoteView) {
    submit_mode(c, q, true).await
}
async fn submit_mode(c: &mut Case, q: &TextQuoteView, bind_now: bool) {
    let request = SubmitJob {
        root_budget_id: q.root_budget_id.clone(),
        invocation_id: q.invocation_id.clone(),
        purpose: InvocationPurpose::Primary,
        quote_id: q.quote_id.clone(),
        request_projection_digest: q.request_projection_digest.clone(),
    };
    let app =
        orange_control_plane::artifacts::http::router(c.f.identity.clone(), c.f.objects.clone());
    let mut http = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/owners/{}/jobs", c.f.owner))
        .body(axum::body::Body::from(
            serde_json::to_vec(&request).expect("JSON"),
        ))
        .expect("request");
    *http.headers_mut() = c.f.headers.clone();
    http.headers_mut()
        .insert("content-type", "application/json".parse().expect("header"));
    http.headers_mut().insert(
        "idempotency-key",
        Uuid::new_v4().to_string().parse().expect("key"),
    );
    let response = app.oneshot(http).await.expect("submit");
    assert_eq!(response.status(), 200);
    let job: orange_protocol::JobView = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("body"),
    )
    .expect("job");
    c.f.job = job.job_id;
    if bind_now {
        bind(&c.f).await;
    }
}
async fn execute_header(
    c: &Case,
    connection: &mut ControlConnection,
    node: &mut NodeJournal,
    journal: &mut ExecutionJournal,
    response: &orange_protocol::model::ResponseCandidate,
) -> worker::OutputHeader {
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let poll = connection
                .poll_execution("priced-test", node, journal)
                .await
                .expect("poll");
            if let Some(a) = poll.assignment {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("assignment");
    let (mut broker, supervisor) = broker(&c.f).await;
    let grant = connection
        .reserve_execution(journal, &mut broker, &supervisor, "cpu", &record.command_id)
        .expect("reserve fixture");
    connection
        .authorize_execution(journal, node, "priced-test", &record.command_id)
        .await
        .expect("cloud grant")
        .confirm(journal, &mut broker, &supervisor)
        .expect("local start fixture");
    let a = assignment(&c.f).await;
    let bytes = serde_json::to_vec(response).expect("JSON");
    worker::OutputHeader {
        transcription_output_version: 0,
        session_id: connection.identity().session_id.clone(),
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
        response_digest: request_digest("worker.cpu.candidate.v1", response).expect("digest"),
    }
}
fn output_frames(
    h: &worker::OutputHeader,
    c: &orange_protocol::model::ResponseCandidate,
) -> Vec<worker::OutputFrame> {
    let bytes = serde_json::to_vec(c).expect("JSON");
    let mut frames = vec![worker::OutputFrame {
        body: Some(worker::output_frame::Body::Header(h.clone())),
    }];
    for (i, part) in bytes.chunks(32768).enumerate() {
        frames.push(worker::OutputFrame {
            body: Some(worker::output_frame::Body::Data(worker::OutputData {
                offset: (i * 32768) as u64,
                data: part.to_vec(),
            })),
        });
    }
    frames
}
fn candidate() -> orange_protocol::model::ResponseCandidate {
    orange_protocol::model::ResponseCandidate {
        response_id: "priced-response".into(),
        text: "Hello".into(),
        tool_calls: vec![],
        native_usage: Some(orange_protocol::model::NativeUsage {
            input_tokens: 999999,
            output_tokens: 999999,
        }),
        finish_reason: "stop".into(),
    }
}
async fn replace_input(c: &mut Case) {
    c.f.request.invocation_id = Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec(&c.f.request).expect("input");
    c.f.upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    c.f.objects
        .begin_upload(&c.f.actor, &c.f.owner, &c.f.upload)
        .await
        .expect("input intent");
    c.f.version =
        c.f.objects
            .put(
                c.f.actor.clone(),
                c.f.owner.clone(),
                c.f.upload.upload_id.clone(),
                tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
            )
            .await
            .expect("actual input")
            .version_id;
}
async fn model_list(
    c: &Case,
    path: &str,
    authenticated: bool,
) -> (axum::http::StatusCode, serde_json::Value) {
    let app =
        orange_control_plane::artifacts::http::router(c.f.identity.clone(), c.f.objects.clone());
    let mut req = axum::http::Request::get(path)
        .body(axum::body::Body::empty())
        .expect("request");
    if authenticated {
        *req.headers_mut() = c.f.headers.clone();
    }
    let reply = app.oneshot(req).await.expect("model listing");
    let status = reply.status();
    assert_eq!(reply.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(reply.into_body(), 128 * 1024)
        .await
        .expect("bounded catalog");
    (
        status,
        serde_json::from_slice(&bytes).expect("JSON response"),
    )
}

#[tokio::test]
async fn disabled_price_cannot_create_a_new_quote_or_reservation() {
    let c = setup_priced("client-cp").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (connection, _, _) = idle(&c, &server).await;
    // More than a full catalog page of enabled prices with an ineligible
    // execution profile must not consume the available-node result bound.
    let disabled_profile = format!("catalog-disabled-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest,enabled) SELECT $1,model_version_id,model_sha256,execution_profile_digest,false FROM reviewed_execution_profiles WHERE execution_profile_id=$2")
        .bind(&disabled_profile).bind(&c.f.profile).execute(c.f.d.store.pool()).await.expect("disabled profile fixture");
    let prefix = format!("0-catalog-{}-", Uuid::new_v4());
    sqlx::query("INSERT INTO text_prices(price_version,node_id,execution_profile_id,meter_profile_digest,input_price,output_price,output_limit,fee_bps) SELECT $1 || i::text,p.node_id,$2,p.meter_profile_digest,p.input_price,p.output_price,p.output_limit,p.fee_bps FROM text_prices p CROSS JOIN generate_series(1,101) i WHERE p.price_version=$3")
        .bind(&prefix).bind(&disabled_profile).bind(&c.price).execute(c.f.d.store.pool()).await.expect("101 enabled but unavailable price fixtures");
    let predecessors: i64 = sqlx::query_scalar("SELECT count(*) FROM text_prices WHERE starts_with(price_version,$1) AND price_version < $2 AND enabled")
        .bind(&prefix).bind(&c.price).fetch_one(c.f.d.store.pool()).await.expect("fixture ordering");
    assert_eq!(
        predecessors, 101,
        "fixture occupies the whole old pre-filter page"
    );
    let catalog =
        c.f.objects
            .prices(&c.f.actor)
            .await
            .expect("current catalog");
    assert!(
        catalog.iter().any(|p| p.price_version == c.price),
        "unavailable records must not starve the live price"
    );
    assert!(catalog.len() <= 100);
    assert!(
        !catalog.iter().any(|p| p.price_version.starts_with(&prefix)),
        "disabled execution profile cannot be offered"
    );
    let (status, document) = model_list(&c, "/api/v1/models", true).await;
    assert_eq!(status, 200);
    let models: orange_protocol::pricing::MarketModelList =
        serde_json::from_value(document).expect("typed HTTP catalogue");
    let [model] = models.data.as_slice() else {
        panic!("one active licensed model expected")
    };
    assert_eq!(model.license, "Apache-2.0");
    assert_eq!(model.repository, "Qwen/Qwen2.5-0.5B-Instruct-GGUF");
    assert_eq!(model.model_sha256, c.meter.descriptor().model_sha256);
    assert_eq!(model.model_version, c.meter.descriptor().model_version);
    assert_eq!(model.context_tokens, c.meter.descriptor().context_tokens);
    assert_eq!(model.max_request_bytes, 262144);
    let offer = model
        .offers
        .iter()
        .find(|offer| offer.price_version == c.price)
        .expect("live price");
    assert_eq!(
        serde_json::to_value(offer).expect("serialize exact-priced tier")["input_price"],
        "2000"
    );
    assert!(
        model
            .offers
            .iter()
            .all(|offer| !offer.price_version.starts_with(&prefix))
    );
    assert_eq!(model_list(&c, "/api/v1/models", false).await.0, 401);
    let (invalid, error) = model_list(&c, "/api/v1/models?owner_id=another", true).await;
    assert_eq!(invalid, 422);
    assert_eq!(error["code"], "VALIDATION_ERROR");
    let req = quote_request(&c, 32);
    let q = quote_http(&c, &req).await;
    sqlx::query("UPDATE text_prices SET enabled=false WHERE price_version=$1")
        .bind(&c.price)
        .execute(c.f.d.store.pool())
        .await
        .expect("admission revoked");
    assert!(
        !c.f.objects
            .prices(&c.f.actor)
            .await
            .expect("revalidated catalog")
            .iter()
            .any(|p| p.price_version == c.price),
        "revoked price disappears from the catalog"
    );
    let (status, document) = model_list(&c, "/api/v1/models", true).await;
    assert_eq!(status, 200);
    let refreshed: orange_protocol::pricing::MarketModelList =
        serde_json::from_value(document).expect("revalidated catalogue");
    assert!(
        refreshed
            .data
            .iter()
            .flat_map(|m| &m.offers)
            .all(|offer| offer.price_version != c.price)
    );
    assert_eq!(
        quote_http(&c, &req).await.quote_id,
        q.quote_id,
        "existing quote fact is not repriced"
    );
    assert!(
        c.f.objects
            .create_text_quote(&c.f.actor, &c.f.owner, &quote_request(&c, 32))
            .await
            .is_err()
    );
    let submit = SubmitJob {
        root_budget_id: q.root_budget_id,
        invocation_id: q.invocation_id,
        purpose: InvocationPurpose::Primary,
        quote_id: q.quote_id,
        request_projection_digest: q.request_projection_digest,
    };
    assert!(
        c.f.d
            .store
            .submit_for_user(&c.f.actor, &c.f.owner, "disabled-price", &submit)
            .await
            .is_err()
    );
    let balance: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&c.f.owner)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("unchanged");
    assert_eq!(balance, (1000, 0));
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}

#[tokio::test]
async fn concurrent_quote_is_one_snapshot_and_equal_bytes_cannot_replace_quoted_version() {
    let mut c = setup_priced("client-cr").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (connection, _, _) = idle(&c, &server).await;
    let req = quote_request(&c, 32);
    let (a, b) = tokio::join!(
        c.f.objects.create_text_quote(&c.f.actor, &c.f.owner, &req),
        c.f.objects.create_text_quote(&c.f.actor, &c.f.owner, &req)
    );
    let q = a.expect("first quote");
    assert_eq!(q.quote_id, b.expect("concurrent same quote").quote_id);
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.entity_id=$1 AND e.event_type='quote.created'").bind(&q.quote_id).fetch_one(c.f.d.store.pool()).await.expect("one event");
    assert_eq!(events, 1);
    let pool = c.f.d.store.pool().clone();
    let artifact: String = sqlx::query_scalar(
        "SELECT artifact_id FROM artifact_uploads WHERE owner_id=$1 AND upload_id=$2",
    )
    .bind(&c.f.owner)
    .bind(&c.f.upload.upload_id)
    .fetch_one(&pool)
    .await
    .expect("quoted input ID");
    let mut funding_barrier = pool.begin().await.expect("private fixture transaction");
    let blocker: i32 =
        sqlx::query_scalar("SELECT pg_backend_pid() FROM wallets WHERE owner_id=$1 FOR UPDATE")
            .bind(&c.f.owner)
            .fetch_one(&mut *funding_barrier)
            .await
            .expect("delay after input check");
    let verify_lock = async {
        tokio::time::timeout(Duration::from_secs(8),async {
            loop {
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
                    .bind(blocker).fetch_one(&pool).await.expect("observe actual funding wait");
                if waiting {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("submission reached the wallet");
        let mut competitor = pool.begin().await.expect("independent deletion lock probe");
        let conflict =
            sqlx::query("SELECT artifact_id FROM artifacts WHERE artifact_id=$1 FOR UPDATE NOWAIT")
                .bind(&artifact)
                .fetch_one(&mut *competitor)
                .await
                .expect_err("quoted source must still be locked");
        assert_eq!(
            conflict
                .as_database_error()
                .and_then(|e| e.code())
                .as_deref(),
            Some("55P03")
        );
        competitor.rollback().await.expect("discard competing lock");
        funding_barrier.commit().await.expect("release test wallet");
    };
    tokio::join!(submit_mode(&mut c, &q, false), verify_lock);
    let (active, fault) = delete_input(&c.f).await;
    assert_eq!(
        active, 409,
        "accepted priced job protects its input before binding"
    );
    assert_eq!(fault["code"], "ARTIFACT_IN_USE");
    let bytes = serde_json::to_vec(&c.f.request).expect("same request");
    let alternate = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    c.f.objects
        .begin_upload(&c.f.actor, &c.f.owner, &alternate)
        .await
        .expect("second artifact");
    let v =
        c.f.objects
            .put(
                c.f.actor.clone(),
                c.f.owner.clone(),
                alternate.upload_id.clone(),
                tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
            )
            .await
            .expect("same bytes different immutable version");
    assert!(
        c.f.objects
            .bind_job_input(
                &c.f.actor,
                &c.f.owner,
                &c.f.job,
                &BindJobInput {
                    upload_id: alternate.upload_id,
                    version_id: v.version_id
                }
            )
            .await
            .is_err()
    );
    bind(&c.f).await;
    c.f.d
        .store
        .cancel_queued(&c.f.owner, &c.f.job, "end-quote-fixture")
        .await
        .expect("queued fixture cleanup");
    assert_eq!(
        delete_input(&c.f).await.0,
        200,
        "terminal use no longer blocks deletion"
    );
    replace_input(&mut c).await;
    let later = quote_http(&c, &quote_request(&c, 32)).await;
    assert_eq!(
        delete_input(&c.f).await.0,
        200,
        "an unused quote does not hold the source"
    );
    let request = SubmitJob {
        root_budget_id: later.root_budget_id,
        invocation_id: later.invocation_id.clone(),
        purpose: InvocationPurpose::Primary,
        quote_id: later.quote_id,
        request_projection_digest: later.request_projection_digest,
    };
    let before: (i64,i64,i64) = sqlx::query_as("SELECT gift,reserved_gift,(SELECT count(*) FROM ledger_transactions WHERE owner_id=$1) FROM wallets WHERE owner_id=$1")
        .bind(&c.f.owner).fetch_one(c.f.d.store.pool()).await.expect("prior credit facts");
    let admission =
        c.f.d
            .store
            .submit_for_user(&c.f.actor, &c.f.owner, "deleted-before-submit", &request)
            .await;
    assert!(matches!(
        admission,
        Err(orange_control_plane::store::StoreError::Forbidden)
    ));
    let after: (i64,i64,i64) = sqlx::query_as("SELECT gift,reserved_gift,(SELECT count(*) FROM ledger_transactions WHERE owner_id=$1) FROM wallets WHERE owner_id=$1")
        .bind(&c.f.owner).fetch_one(c.f.d.store.pool()).await.expect("no frozen credits or posting");
    assert_eq!(before, after);
    let residue: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM jobs WHERE owner_id=$1 AND invocation_id=$2),
        (SELECT reserved FROM budget_roots WHERE root_budget_id=$3)",
    )
    .bind(&c.f.owner)
    .bind(later.invocation_id)
    .bind(&c.budget)
    .fetch_one(c.f.d.store.pool())
    .await
    .expect("job and budget");
    assert_eq!(residue, (0, 0));
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}
#[tokio::test]
async fn wrong_tool_arguments_cannot_mint_usage_or_a_charge() {
    rejected_candidate(RejectedInput::ToolArguments).await;
}
#[tokio::test]
async fn malformed_candidate_is_recorded_once_without_financial_effects() {
    rejected_candidate(RejectedInput::Malformed).await;
}
#[tokio::test]
async fn claimed_unknown_owner_cannot_assign_credit_or_redirect_review() {
    rejected_candidate(RejectedInput::UnknownOwner).await;
}
enum RejectedInput {
    ToolArguments,
    Malformed,
    UnknownOwner,
}
async fn rejected_candidate(kind: RejectedInput) {
    let mut c = setup_priced(match kind {
        RejectedInput::ToolArguments => "client-cq",
        RejectedInput::Malformed => "client-el",
        RejectedInput::UnknownOwner => "client-ej",
    })
    .await;
    c.f.request.tools = vec![orange_protocol::model::ToolDefinition {
        name: "read_file".into(),
        description: "Read a consented file".into(),
        parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string","minLength":1}},"required":["path"],"additionalProperties":false}),
    }];
    replace_input(&mut c).await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (mut connection, mut node, mut journal) = idle(&c, &server).await;
    let q = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &q).await;
    let mut response = candidate();
    response.text = String::new();
    response.finish_reason = "tool_calls".into();
    response.tool_calls = vec![orange_protocol::model::ToolCall {
        id: "call".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path":7}),
    }];
    let mut h = execute_header(&c, &mut connection, &mut node, &mut journal, &response).await;
    let mut raw = server.client(Some(&c.f.d.certificate)).await.expect("TLS");
    let bytes = match kind {
        RejectedInput::Malformed => Some(b"{".to_vec()),
        RejectedInput::UnknownOwner => {
            let mut input = serde_json::to_value(candidate()).expect("valid response");
            input["owner_id"] = serde_json::json!("missing-fixture-owner");
            Some(serde_json::to_vec(&input).expect("caller-proposed owner"))
        }
        RejectedInput::ToolArguments => None,
    };
    let frames = if let Some(bytes) = bytes {
        h.size_bytes = bytes.len() as u64;
        h.sha256 = hex::encode(Sha256::digest(&bytes));
        vec![
            worker::OutputFrame {
                body: Some(worker::output_frame::Body::Header(h.clone())),
            },
            worker::OutputFrame {
                body: Some(worker::output_frame::Body::Data(worker::OutputData {
                    offset: 0,
                    data: bytes,
                })),
            },
        ]
    } else {
        output_frames(&h, &response)
    };
    for _ in 0..2 {
        assert!(
            raw.commit_output(tokio_stream::iter(frames.clone()))
                .await
                .is_err()
        );
    }
    let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM job_usage_facts WHERE job_id=$1")
        .bind(&c.f.job)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("facts");
    assert_eq!(facts, 0);
    let case:(String,String,String)=sqlx::query_as("SELECT r.upload_id,r.owner_id,c.business_ref FROM metering_input_rejections r JOIN reconciliation_cases c USING(case_id) WHERE r.job_id=$1")
        .bind(&c.f.job).fetch_one(c.f.d.store.pool()).await.expect("auditable pre-publication rejection");
    if matches!(kind, RejectedInput::UnknownOwner) {
        let reason: String =
            sqlx::query_scalar("SELECT reason FROM metering_input_rejections WHERE upload_id=$1")
                .bind(&h.upload_id)
                .fetch_one(c.f.d.store.pool())
                .await
                .expect("classified unknown owner claim");
        assert_eq!(reason, "untrusted_owner");
        let foreign: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ledger_transactions WHERE owner_id='missing-fixture-owner'",
        )
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("foreign accounting");
        assert_eq!(foreign, 0);
    }
    assert_eq!(
        case,
        (
            h.upload_id.clone(),
            c.f.owner.clone(),
            format!("metering-input:{}", h.upload_id)
        )
    );
    assert!(
        c.f.d
            .store
            .job(&c.f.owner, &c.f.job)
            .await
            .expect("job")
            .charge
            .is_none()
    );
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}
#[tokio::test]
async fn actual_counter_quote_output_and_atomic_credit_settlement_ignore_provider_counts() {
    let mut c = setup_priced("client-cl").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (mut connection, mut node, mut journal) = idle(&c, &server).await;
    let req = quote_request(&c, 32);
    let quote = quote_http(&c, &req).await;
    let again = quote_http(&c, &req).await;
    assert_eq!(quote.quote_id, again.quote_id);
    let measured = c
        .meter
        .input(c.f.request.clone())
        .await
        .expect("independent actual count");
    assert_eq!(quote.input_units, measured.units().to_string());
    submit(&mut c, &quote).await;
    let response = candidate();
    let h = execute_header(&c, &mut connection, &mut node, &mut journal, &response).await;
    let mut raw = server.client(Some(&c.f.d.certificate)).await.expect("TLS");
    let receipt = raw
        .commit_output(tokio_stream::iter(output_frames(&h, &response)))
        .await
        .expect("measure, publish and settle")
        .into_inner();
    let units: i64 = sqlx::query_scalar("SELECT output_units FROM job_usage_facts WHERE job_id=$1")
        .bind(&c.f.job)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("server measurement");
    assert!(units > 0 && units < 32);
    assert_ne!(units, 999999);
    let expected = (measured.units() * 2000).div_ceil(1000) + (units as u64 * 6000).div_ceil(1000);
    let job = c.f.d.store.job(&c.f.owner, &c.f.job).await.expect("job");
    assert_eq!(job.settlement_state, "settled");
    assert_eq!(
        job.charge.expect("charge").0.to_string(),
        expected.to_string()
    );
    let balance: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&c.f.owner)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("buyer");
    assert_eq!(balance, (1000 - expected as i64, 0));
    let earned: i64 = sqlx::query_scalar("SELECT pending_earned FROM wallets WHERE owner_id=$1")
        .bind(&c.f.d.owner)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("pending provider");
    assert_eq!(earned, expected as i64 - (expected as i64 * 2000 / 10000));
    let recovered = raw
        .query_output(worker::OutputQuery {
            session_id: connection.identity().session_id.clone(),
            original: Some(h.clone()),
        })
        .await
        .expect("query receipt")
        .into_inner();
    assert_eq!(receipt, recovered);
    raw.commit_output(tokio_stream::iter(output_frames(&h, &response)))
        .await
        .expect("same output replay after settlement");
    let (a, b) = tokio::join!(c.f.objects.settle_ready(100), c.f.objects.settle_ready(100));
    assert_eq!(a.expect("retry"), (0, 0));
    assert_eq!(b.expect("retry"), (0, 0));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(format!("settle:{}", c.f.job))
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("once");
    assert_eq!(count, 1);
    let (basis,interval,anchored):(String,i64,bool)=sqlx::query_as(
        "SELECT r.basis,extract(epoch FROM r.due_at-r.anchor_at)::bigint,r.anchor_at=c.committed_at
         FROM job_response_commits c CROSS JOIN LATERAL orange_artifact_retention(c.artifact_id) r WHERE c.job_id=$1"
    ).bind(&c.f.job).fetch_one(c.f.d.store.pool()).await.expect("delivered output retention clock");
    assert_eq!(
        (basis.as_str(), interval, anchored),
        ("response_commit", 86400, true)
    );
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}
#[tokio::test]
async fn canonical_limit_is_checked_before_response_publication_and_charge() {
    let mut c = setup_priced("client-cm").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (mut connection, mut node, mut journal) = idle(&c, &server).await;
    let q = quote_http(&c, &quote_request(&c, 1)).await;
    submit(&mut c, &q).await;
    let mut response = candidate();
    response.text = "one two three four five six seven".into();
    let h = execute_header(&c, &mut connection, &mut node, &mut journal, &response).await;
    let mut raw = server.client(Some(&c.f.d.certificate)).await.expect("TLS");
    assert!(
        raw.commit_output(tokio_stream::iter(output_frames(&h, &response)))
            .await
            .is_err()
    );
    let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM job_usage_facts WHERE job_id=$1")
        .bind(&c.f.job)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("no fact");
    assert_eq!(facts, 0);
    let job = c.f.d.store.job(&c.f.owner, &c.f.job).await.expect("job");
    assert_eq!(job.execution_state, "running");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}

#[tokio::test]
async fn settlement_failure_is_atomic_and_competing_recovery_cannot_double_charge() {
    failed_settlement("client-cn", false).await;
}

#[tokio::test]
async fn metering_deadline_preserves_hold_and_requires_independent_replay() {
    failed_settlement("client-ek", true).await;
}

use crate::admin_browser;
use crate::finance_sessions;

async fn failed_settlement(cert: &str, expire: bool) {
    let mut c = setup_priced(cert).await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (mut connection, mut node, mut journal) = idle(&c, &server).await;
    let q = quote_http(&c, &quote_request(&c, 32)).await;
    submit(&mut c, &q).await;
    let response = candidate();
    let h = execute_header(&c, &mut connection, &mut node, &mut journal, &response).await;
    let job = Uuid::parse_str(&c.f.job).expect("generated job");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION settle_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM ledger_transactions WHERE transaction_id=NEW.transaction_id AND business_key='settle:{job}') THEN RAISE EXCEPTION 'explicit settlement fault fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER settle_fault_{suffix} BEFORE INSERT ON ledger_entries FOR EACH ROW EXECUTE FUNCTION settle_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(c.f.d.store.pool())
        .await
        .expect("isolated fault");
    let mut raw = server.client(Some(&c.f.d.certificate)).await.expect("TLS");
    let result = raw
        .commit_output(tokio_stream::iter(output_frames(&h, &response)))
        .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER settle_fault_{suffix} ON ledger_entries; DROP FUNCTION settle_fault_{suffix}();"))).execute(c.f.d.store.pool()).await.expect("remove exact fault");
    assert!(result.is_err());
    let job = c.f.d.store.job(&c.f.owner, &c.f.job).await.expect("job");
    assert_eq!(job.execution_state, "succeeded");
    assert_eq!(job.settlement_state, "pending");
    assert!(job.charge.is_none());
    let before: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&c.f.owner)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("no half debit");
    assert_eq!(
        before,
        (1000 - q.max_amount.0.units(), q.max_amount.0.units())
    );
    let pending: i64 = sqlx::query_scalar("SELECT pending_earned FROM wallets WHERE owner_id=$1")
        .bind(&c.f.d.owner)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("no half credit");
    assert_eq!(pending, 0);
    let key = format!("settle:{}", c.f.job);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(&key)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("no half ledger");
    assert_eq!(count, 0);
    let phases = "SELECT h.stage FROM metering_receipt_history h JOIN metering_receipts r USING(event_id) WHERE r.job_id=$1 ORDER BY h.sequence_id";
    let before_retry: Vec<String> = sqlx::query_scalar(phases)
        .bind(&c.f.job)
        .fetch_all(c.f.d.store.pool())
        .await
        .expect("read failed attempt history");
    assert_eq!(before_retry, ["received", "validated", "retry"]);
    if expire {
        // Only the retry clock moves; the immutable receipt, original quote and held funds remain intact.
        sqlx::query("UPDATE metering_receipts SET deadline=clock_timestamp() WHERE job_id=$1")
            .bind(&c.f.job)
            .execute(c.f.d.store.pool())
            .await
            .expect("advance isolated retry clock");
        assert_eq!(
            c.f.objects
                .settle_ready(100)
                .await
                .expect("expire the outstanding receipt")
                .1,
            1
        );
        assert_eq!(
            c.f.objects
                .settle_ready(100)
                .await
                .expect("skip already dead receipt")
                .1,
            0
        );
        let (case_id,event_id):(String,String)=sqlx::query_as("SELECT case_id,event_id FROM metering_receipts WHERE job_id=$1 AND state='dead' AND transaction_id IS NULL")
             .bind(&c.f.job).fetch_one(c.f.d.store.pool()).await.expect("read original event and scoped case");
        let after: (i64, i64) =
            sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
                .bind(&c.f.owner)
                .fetch_one(c.f.d.store.pool())
                .await
                .expect("read preserved wallet");
        assert_eq!(after, before, "no release of unknown charge");
        let staff = finance_sessions::Admin::new(&c.f.d.store, &c.f.owner).await;
        assert_ne!(staff.maker.id, staff.checker.id);
        let query = format!(
            "/admin/v1/reconciliation-cases/{case_id}/activity?environment=dev&scope_kind=tenant&scope_id={}",
            c.f.owner
        );
        let (view, _) = staff
            .call(
                &staff.maker,
                "GET",
                &query,
                "",
                serde_json::Value::Null,
                200,
            )
            .await;
        assert_eq!(view["metering"]["event_id"], event_id);
        assert_eq!(view["metering"]["state"], "dead");
        assert_eq!(view["metering"]["last_failure"], "retry_deadline");
        let request = serde_json::json!({"scope":{"environment":"dev","kind":"tenant","scope_id":c.f.owner},"target_version":view["version"],"ticket_id":"metering-review","reason":"Independently reviewed original server metering receipt","action_type":"case.update","payload":{"change":"retry_metering","case_id":case_id,"event_id":event_id}});
        let mut wrong = request.clone();
        wrong["payload"]["event_id"] = serde_json::json!("different-original-event");
        staff
            .call(&staff.maker, "POST", "/admin/v1/actions", "", wrong, 409)
            .await;
        let mut close = request.clone();
        close["payload"] = serde_json::json!({"change":"close","case_id":case_id,"resolution_ref":"premature-close"});
        staff
            .call(&staff.maker, "POST", "/admin/v1/actions", "", close, 409)
            .await;
        if let Ok(driver) = std::env::var("ORANGE_METERING_ADMIN_BROWSER") {
            admin_browser::run(staff.app.clone(),&std::env::var("ORANGE_METERING_ADMIN_NODE").expect("pinned Admin browser Node"),&driver,serde_json::json!({
                "origin":staff.origin,"maker":staff.maker.token,"checker":staff.checker.token,
                "case":case_id,"event":event_id,"screenshot":std::env::var("ORANGE_METERING_SCREENSHOT").expect("metering screenshot destination")
            })).await;
        } else {
            let (action, tag) = staff
                .call(&staff.maker, "POST", "/admin/v1/actions", "", request, 201)
                .await;
            let path = format!(
                "/admin/v1/actions/{}",
                action["action_id"].as_str().expect("created action ID")
            );
            let approve = serde_json::json!({"decision":"approve","payload_digest":action["payload_digest"],"policy_version":action["policy_version"],"reason":"Checked original event and budget ownership"});
            staff
                .call(
                    &staff.maker,
                    "POST",
                    &format!("{path}/approvals"),
                    &tag,
                    approve.clone(),
                    403,
                )
                .await;
            let (_, tag) = staff
                .call(
                    &staff.checker,
                    "POST",
                    &format!("{path}/approvals"),
                    &tag,
                    approve,
                    200,
                )
                .await;
            let execute = serde_json::json!({"payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
            staff
                .call(
                    &staff.maker,
                    "POST",
                    &format!("{path}/execute"),
                    &tag,
                    execute,
                    202,
                )
                .await;
        }
        let (view, _) = staff
            .call(
                &staff.maker,
                "GET",
                &query,
                "",
                serde_json::Value::Null,
                200,
            )
            .await;
        assert_eq!(view["metering"]["state"], "received");
        assert_eq!(view["metering"]["generation"], "2");
        assert_eq!(view["metering"]["attempts"], "0");
    }
    let (a, b) = tokio::join!(c.f.objects.settle_ready(100), c.f.objects.settle_ready(100));
    assert_eq!(a.expect("recovery").1, 0);
    assert_eq!(b.expect("competing recovery").1, 0);
    assert_eq!(
        c.f.d
            .store
            .job(&c.f.owner, &c.f.job)
            .await
            .expect("settled")
            .settlement_state,
        "settled"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE business_key=$1")
            .bind(&key)
            .fetch_one(c.f.d.store.pool())
            .await
            .expect("once");
    assert_eq!(count, 1);
    let history: Vec<String> = sqlx::query_scalar(phases)
        .bind(&c.f.job)
        .fetch_all(c.f.d.store.pool())
        .await
        .expect("read terminal event history");
    if expire {
        assert_eq!(
            history,
            [
                "received",
                "validated",
                "retry",
                "dead",
                "reopened",
                "validated",
                "posted"
            ]
        );
    } else {
        assert_eq!(history, ["received", "validated", "retry", "posted"]);
    }
    let registered:bool=sqlx::query_scalar("SELECT r.event_id=c.commit_id AND r.transaction_id=s.transaction_id AND r.response_digest=c.response_digest AND r.attempts=$2
        FROM metering_receipts r JOIN job_response_commits c USING(job_id) JOIN job_settlements s USING(job_id) WHERE r.job_id=$1").bind(&c.f.job).bind(if expire{1i32}else{2}).fetch_one(c.f.d.store.pool()).await.expect("match original posting");
    assert!(
        registered,
        "retry retained the original event and actual posting"
    );

    let balance:i64=sqlx::query_scalar("SELECT sum(e.amount)::bigint FROM ledger_entries e JOIN ledger_transactions t USING(transaction_id) WHERE t.business_key=$1").bind(&key).fetch_one(c.f.d.store.pool()).await.expect("balanced");
    assert_eq!(balance, 0);
    let prior_session = connection.identity().session_id.clone();
    connection.close();
    let restored = ControlConnection::connect_output_execution(
        worker_config(&server, &c.f.d),
        &mut node,
        &mut journal,
    )
    .await
    .expect("fresh authorized connection after financial review");
    assert_ne!(prior_session, restored.identity().session_id);
    raw.query_output(worker::OutputQuery {
        session_id: restored.identity().session_id.clone(),
        original: Some(h),
    })
    .await
    .expect("recover original delivered fact through current session");
    restored.close();
    server.stop().await;
    c.f.objects.drain().await.expect("drain");
    c.meter.shutdown().await.expect("counter exited");
}

#[tokio::test]
async fn actual_worker_counter_user_api_quote_delivery_and_settlement() {
    use super::host_cases as host;
    use std::{os::unix::fs::PermissionsExt, process::Stdio};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
    };
    let c = setup_priced("client-co").await;
    let measured = c
        .meter
        .input(c.f.request.clone())
        .await
        .expect("actual preflight oracle");
    let qreq = quote_request(&c, 32);
    let p = profile();
    let worker_root = runtime();
    let meter_root = runtime();
    let worker_config = host::configuration_port(&c.f, 1, &p, worker_root.path());
    host::enable_outputs(&worker_config);
    let config_root = runtime();
    let tls = PathBuf::from(std::env::var("ORANGE_WORKER_TLS_DIR").expect("TLS"));
    let gateway_config = config_root.path().join("gateway.json");
    let meter_config = config_root.path().join("meter.json");
    for (path, value) in [
        (
            &gateway_config,
            serde_json::json!({"gateway_instance_id":Uuid::new_v4().to_string(),"server_certificate_file":tls.join("server.pem"),"server_key_file":tls.join("server.key"),"client_ca_file":tls.join("ca.pem")}),
        ),
        (
            &meter_config,
            serde_json::json!({"protocol":1,"runtime_directory":meter_root.path(),"executable":p.executable,"executable_sha256":p.executable_sha256,"model":p.model,"model_sha256":p.model_sha256,"model_bytes":p.model_bytes,"model_version":p.model_version,"context_tokens":p.context_tokens}),
        ),
    ] {
        std::fs::write(path, serde_json::to_vec(&value).expect("JSON")).expect("private config");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("private");
    }
    c.f.objects.drain().await.expect("old writer drain");
    drop(c.f.objects);
    c.meter.shutdown().await.expect("old counter stopped");
    let mut api = Command::new(std::env::var("ORANGE_USER_API_BINARY").expect("actual API"))
        .env_clear()
        .env("ORANGE_ENV", "development")
        .env(
            "DATABASE_URL",
            std::env::var("ORANGE_TEST_DATABASE_URL").expect("DB"),
        )
        .env(
            "ORANGE_USER_CONFIG_FILE",
            std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("identity"),
        )
        .env("ORANGE_USER_BIND", "127.0.0.1:0")
        .env("ORANGE_ARTIFACT_ROOT", &c.f.root)
        .env("ORANGE_ARTIFACT_BYTE_LIMIT", "1048576")
        .env("ORANGE_ARTIFACT_RECORD_LIMIT", "100")
        .env("ORANGE_WORKER_GATEWAY_CONFIG", &gateway_config)
        .env("ORANGE_WORKER_GATEWAY_BIND", "127.0.0.1:0")
        .env("ORANGE_METER_CONFIG", &meter_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("API/counter process");
    let mut logs = BufReader::new(api.stderr.take().expect("diagnostics")).lines();
    let (http, grpc) = tokio::time::timeout(Duration::from_secs(35), async {
        let mut http = None;
        let mut grpc = None;
        loop {
            let line = logs.next_line().await.expect("read").expect("live API");
            if let Some(v) = line.strip_prefix("Development User API listening on ") {
                http = Some(
                    v.split(';')
                        .next()
                        .expect("address")
                        .parse::<std::net::SocketAddr>()
                        .expect("address"),
                );
            }
            if let Some(v) = line.strip_prefix("Development output Gateway listening on ") {
                grpc = Some(v.parse::<std::net::SocketAddr>().expect("address"));
            }
            if let (Some(h), Some(g)) = (http, grpc) {
                break (h, g);
            }
        }
    })
    .await
    .expect("actual listeners");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&worker_config).expect("worker config"))
            .expect("JSON");
    value["endpoint"] = serde_json::json!(format!("https://{grpc}/"));
    std::fs::write(&worker_config, serde_json::to_vec(&value).expect("JSON")).expect("endpoint");
    let (mut worker, mut events) = host::start(&worker_config);
    host::event(&mut events, "ready").await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("client");
    let base = format!("http://{http}/api/v1/owners/{}", c.f.owner);
    let mut headers = c.f.headers.clone();
    headers.insert("idempotency-key", qreq.request_id.parse().expect("id"));
    let quote: TextQuoteView = client
        .post(format!("{base}/text-quotes"))
        .headers(headers.clone())
        .json(&qreq)
        .send()
        .await
        .expect("quote HTTP")
        .error_for_status()
        .expect("trusted quote")
        .json()
        .await
        .expect("quote");
    assert_eq!(quote.input_units, measured.units().to_string());
    headers.insert(
        "idempotency-key",
        Uuid::new_v4().to_string().parse().expect("id"),
    );
    let job: orange_protocol::JobView = client
        .post(format!("{base}/jobs"))
        .headers(headers.clone())
        .json(&SubmitJob {
            root_budget_id: quote.root_budget_id,
            invocation_id: quote.invocation_id,
            purpose: InvocationPurpose::Primary,
            quote_id: quote.quote_id,
            request_projection_digest: quote.request_projection_digest,
        })
        .send()
        .await
        .expect("submit")
        .error_for_status()
        .expect("authorized")
        .json()
        .await
        .expect("job");
    headers.insert("idempotency-key", job.job_id.parse().expect("id"));
    client
        .post(format!("{base}/jobs/{}/input", job.job_id))
        .headers(headers.clone())
        .json(&BindJobInput {
            upload_id: c.f.upload.upload_id.clone(),
            version_id: c.f.version.clone(),
        })
        .send()
        .await
        .expect("bind")
        .error_for_status()
        .expect("authorized input");
    host::event(&mut events, "response_committed").await;
    let result: orange_protocol::JobView = client
        .get(format!("{base}/jobs/{}", job.job_id))
        .headers(headers.clone())
        .send()
        .await
        .expect("status")
        .error_for_status()
        .expect("status")
        .json()
        .await
        .expect("job");
    assert_eq!(result.settlement_state, "settled");
    assert!(result.charge.expect("charge").0.units() > 0);
    let view: orange_protocol::artifacts::ArtifactView = client
        .get(format!("{base}/jobs/{}/result", job.job_id))
        .headers(headers.clone())
        .send()
        .await
        .expect("result")
        .error_for_status()
        .expect("result")
        .json()
        .await
        .expect("view");
    let bytes = client
        .get(format!(
            "{base}/uploads/{}/content?version_id={}",
            view.upload_id, view.version_id
        ))
        .headers(headers)
        .send()
        .await
        .expect("read")
        .error_for_status()
        .expect("read")
        .bytes()
        .await
        .expect("bytes");
    let response: orange_protocol::model::ResponseCandidate =
        serde_json::from_slice(&bytes).expect("actual model");
    assert!(!response.text.trim().is_empty());
    let oracle = TrustedMeter::launch(c._meter_root.path(), profile())
        .await
        .expect("independent fixed counter");
    let expected = oracle
        .output(c.f.request.clone(), response, 32)
        .await
        .expect("actual canonical units");
    oracle.shutdown().await.expect("oracle stopped");
    let units: i64 = sqlx::query_scalar("SELECT output_units FROM job_usage_facts WHERE job_id=$1")
        .bind(&job.job_id)
        .fetch_one(c.f.d.store.pool())
        .await
        .expect("usage");
    assert_eq!(units as u64, expected.units());
    host::terminate(&mut worker).await;
    host::stopped(worker_root.path());
    assert!(api.try_wait().expect("owned API").is_none());
    let pid =
        rustix::process::Pid::from_raw(api.id().expect("owned live child") as i32).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::INT)
        .expect("owned API graceful stop");
    assert!(
        tokio::time::timeout(Duration::from_secs(20), api.wait())
            .await
            .expect("counter/I/O stopped")
            .expect("wait")
            .success()
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(meter_root.path().join("process.json")).expect("counter stop receipt"),
    )
    .expect("JSON");
    assert_eq!(state["phase"], "stopped");
}

#[tokio::test]
async fn original_quote_funding_tracks_other_reservations_without_spending() {
    let c = setup_priced("client-ed").await;
    let server = Running::start_with_output(&c.f.d.store, c.f.objects.clone()).await;
    let (connection, _node, _journal) = idle(&c, &server).await;
    let q = quote_http(&c, &quote_request(&c, 32)).await;
    let db = &c.f.d.store;
    let owner = &c.f.owner;
    let app =
        orange_control_plane::artifacts::http::router(c.f.identity.clone(), c.f.objects.clone());
    let read = || async {
        let mut http = axum::http::Request::builder()
            .uri(format!(
                "/api/v1/owners/{owner}/quotes/{}/funding",
                q.quote_id
            ))
            .body(axum::body::Body::empty())
            .expect("request");
        *http.headers_mut() = c.f.headers.clone();
        let response = app.clone().oneshot(http).await.expect("real router");
        assert_eq!(response.status(), 200);
        serde_json::from_slice::<serde_json::Value>(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .expect("body"),
        )
        .expect("funding")
    };
    let initial = read().await;
    assert_eq!(initial["status"], "sufficient");
    let amount = initial["wallet_available"].as_str().expect("own funds");
    let separate = db
        .create_budget(
            owner,
            "funding-other-root",
            &orange_protocol::CreateBudget {
                scope_id: "separate-funding-scope".into(),
                limit: amount.to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("separate budget");
    for (root, expected) in [
        (&separate.root_budget_id, "insufficient_funds"),
        (&q.root_budget_id, "insufficient_budget"),
    ] {
        let reservation = orange_protocol::SubmitJob {
            root_budget_id: root.clone(),
            invocation_id: Uuid::new_v4().to_string(),
            quote_id: Uuid::new_v4().to_string(),
            purpose: orange_protocol::InvocationPurpose::Primary,
            request_projection_digest: "1".repeat(64),
        };
        sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at)
          VALUES($1,$2,$3,$4,$5,'funding-reservation-fixture',$6,clock_timestamp()+interval '1 minute')")
          .bind(&reservation.quote_id).bind(owner).bind(root).bind(&reservation.invocation_id).bind(&reservation.request_projection_digest)
          .bind(amount.parse::<i64>().expect("balance")).execute(db.pool()).await.expect("fixed competing quote");
        let job = db
            .submit(owner, &reservation.invocation_id, &reservation)
            .await
            .expect("real competing hold");
        let before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE owner_id=$1")
                .bind(owner)
                .fetch_one(db.pool())
                .await
                .expect("entries");
        let updated = read().await;
        assert_eq!(updated["status"], expected);
        assert_eq!(updated["wallet_available"], "0");
        let after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ledger_transactions WHERE owner_id=$1")
                .bind(owner)
                .fetch_one(db.pool())
                .await
                .expect("entries");
        assert_eq!(before, after);
        db.cancel_queued(owner, &job.job_id, &format!("cancel-{}", job.job_id))
            .await
            .expect("release competitor");
        let restored = read().await;
        assert_eq!(restored["status"], "sufficient");
        assert_eq!(restored["wallet_available"], amount);
    }
    connection.close();
    server.stop().await;
    c.f.objects.drain().await.expect("closed objects");
    c.meter.shutdown().await.expect("closed counter");
}
