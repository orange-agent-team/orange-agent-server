//! Actual PG/HTTP barrier races. Identity/quote/enrollment are fixtures.
use super::*;
use orange_control_plane::store::StoreError;
type AdvisoryTag = (i64, i64, i16);
async fn held_tag(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> AdvisoryTag {
    let tags: Vec<AdvisoryTag> = sqlx::query_as("SELECT classid::bigint,objid::bigint,objsubid FROM pg_locks WHERE pid=pg_backend_pid() AND locktype='advisory' AND granted")
        .fetch_all(&mut **tx).await.expect("held transaction lock identity");
    assert_eq!(tags.len(), 1, "fixture holds exactly one advisory lock");
    tags[0]
}
async fn waiter_count(pool: &sqlx::PgPool, tag: AdvisoryTag) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND locktype='advisory' AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=$3 AND NOT granted")
        .bind(tag.0).bind(tag.1).bind(tag.2).fetch_one(pool).await.expect("waiters for this exact lock")
}
async fn waiters(pool: &sqlx::PgPool, tag: AdvisoryTag, count: i64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if waiter_count(pool, tag).await == count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("required snapshots queued behind this exact transaction lock");
}
async fn pending(f: &InputFixture) -> SubmitJob {
    f.d.store
        .cancel_for_user(&f.actor, &f.owner, &f.job, "retire-initial")
        .await
        .expect("retire fixture");
    let root =
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("root")
            .root_budget_id;
    let mut input = f.request.clone();
    input.invocation_id = Uuid::new_v4().to_string();
    let quote = Uuid::new_v4().to_string();
    let projection = request_digest("model-request-v1", &input).expect("digest");
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,$6,100,clock_timestamp()+interval '1 hour')").bind(&quote).bind(&f.owner).bind(&root).bind(&input.invocation_id).bind(&projection).bind(Uuid::new_v4().to_string()).execute(f.d.store.pool()).await.expect("explicit personal quote fixture");
    SubmitJob {
        root_budget_id: root,
        invocation_id: input.invocation_id,
        purpose: InvocationPurpose::Primary,
        quote_id: quote,
        request_projection_digest: projection,
    }
}
async fn withdraw(
    f: &InputFixture,
    request: &SubmitJob,
    key: &str,
    headers: axum::http::HeaderMap,
) -> (u16, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut http = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/owners/{}/invocations/{}/withdraw",
            f.owner, request.invocation_id
        ))
        .body(axum::body::Body::from(
            serde_json::to_vec(request).expect("body"),
        ))
        .expect("HTTP");
    *http.headers_mut() = headers;
    http.headers_mut()
        .insert("content-type", "application/json".parse().expect("type"));
    http.headers_mut()
        .insert("idempotency-key", key.parse().expect("key"));
    let response = app.oneshot(http).await.expect("route");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}
#[tokio::test]
async fn prevented_submission_is_immutable_and_never_reserves_or_claims_cleanup() {
    let f = setup("client-dl").await;
    let request = pending(&f).await;
    let first = withdraw(&f, &request, "original-withdraw", f.headers.clone()).await;
    assert_eq!(first.0, 200);
    assert_eq!(first.1["prevented"], true);
    assert!(first.1["job"].is_null());
    assert_eq!(
        withdraw(&f, &request, "original-withdraw", f.headers.clone()).await,
        first
    );
    assert!(matches!(
        f.d.store
            .submit_for_user(&f.actor, &f.owner, "late-submit", &request)
            .await,
        Err(StoreError::InvocationWithdrawn)
    ));
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE owner_id=$1 AND invocation_id=$2")
            .bind(&f.owner)
            .bind(&request.invocation_id)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no Job");
    assert_eq!(rows, 0);
    let balance: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("unchanged wallet");
    assert_eq!(balance, (1000, 0));
    let mut conflict = request.clone();
    conflict.request_projection_digest = "0".repeat(64);
    assert_eq!(
        withdraw(&f, &conflict, "original-withdraw", f.headers.clone())
            .await
            .0,
        409
    );
    assert!(
        sqlx::query("DELETE FROM invocation_withdrawals WHERE owner_id=$1")
            .bind(&f.owner)
            .execute(f.d.store.pool())
            .await
            .is_err(),
        "barrier immutable"
    );
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn withdrawal_rechecks_authentication_and_csrf_even_on_original_receipt() {
    let f = setup("client-dm").await;
    let other = setup("client-dn").await;
    let request = pending(&f).await;
    assert_eq!(
        withdraw(&f, &request, "withdraw", other.headers.clone())
            .await
            .0,
        403
    );
    let mut bad = f.headers.clone();
    bad.remove("x-csrf-token");
    assert_eq!(withdraw(&f, &request, "withdraw", bad).await.0, 403);
    assert_eq!(
        withdraw(&f, &request, "withdraw", f.headers.clone())
            .await
            .0,
        200
    );
    sqlx::query("UPDATE user_sessions SET revoked=true WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("revoke");
    assert_eq!(
        withdraw(&f, &request, "withdraw", f.headers.clone())
            .await
            .0,
        401
    );
    f.objects.drain().await.expect("drain");
    other.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn accepted_job_uses_existing_cancellation_without_inventing_prevention() {
    let f = setup("client-do").await;
    let request = pending(&f).await;
    let job =
        f.d.store
            .submit_for_user(&f.actor, &f.owner, "original-submit", &request)
            .await
            .expect("actual reservation");
    let stopped = withdraw(&f, &request, "original-stop", f.headers.clone()).await;
    assert_eq!(stopped.0, 200);
    assert_eq!(stopped.1["prevented"], false);
    assert!(stopped.1["receipt_id"].is_null());
    assert_eq!(stopped.1["job"]["job_id"], job.job_id);
    assert_eq!(stopped.1["job"]["execution_state"], "cancelled");
    assert_eq!(stopped.1["job"]["charge"], "0");
    assert_eq!(
        withdraw(&f, &request, "original-stop", f.headers.clone()).await,
        stopped
    );
    let barriers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invocation_withdrawals WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no fake pre-admission receipt");
    assert_eq!(barriers, 0);
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn concurrent_submit_and_withdraw_leave_either_prevented_or_one_cancelled_job() {
    let f = setup("client-dp").await;
    for _ in 0..12 {
        let request = pending(&f).await;
        let submit_key = Uuid::new_v4().to_string();
        let stop_key = Uuid::new_v4().to_string();
        let (submitted, stopped) = tokio::join!(
            f.d.store
                .submit_for_user(&f.actor, &f.owner, &submit_key, &request),
            f.d.store
                .withdraw_invocation(&f.actor, &f.owner, &stop_key, &request)
        );
        let stopped = stopped.expect("serialized withdrawal");
        if stopped.prevented {
            assert!(matches!(submitted, Err(StoreError::InvocationWithdrawn)));
        } else {
            let job = stopped.job.expect("real Job");
            assert_eq!(job.execution_state, "cancelled");
            assert_eq!(job.settlement_state, "settled");
        }
        let balance: (i64, i64) =
            sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
                .bind(&f.owner)
                .fetch_one(f.d.store.pool())
                .await
                .expect("conserved wallet");
        assert_eq!(balance, (1000, 0));
    }
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn failed_withdrawal_event_rolls_back_barrier_and_does_not_block_original_submission() {
    let f = setup("client-dq").await;
    let request = pending(&f).await;
    let invocation = Uuid::parse_str(&request.invocation_id).expect("fixture UUID");
    let suffix = Uuid::new_v4().simple().to_string();
    let ddl = format!(
        "CREATE FUNCTION withdraw_fault_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.event_type='invocation.withdrawn_before_acceptance' AND EXISTS(SELECT 1 FROM event_streams WHERE stream_id=NEW.stream_id AND entity_id='{invocation}') THEN RAISE EXCEPTION 'explicit withdrawal event fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER withdraw_fault_{suffix} BEFORE INSERT ON durable_events FOR EACH ROW EXECUTE FUNCTION withdraw_fault_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("scoped fault");
    let result = withdraw(&f, &request, "withdraw-fault", f.headers.clone()).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER withdraw_fault_{suffix} ON durable_events; DROP FUNCTION withdraw_fault_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact fault");
    assert_eq!(result.0, 503);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invocation_withdrawals WHERE owner_id=$1 AND invocation_id=$2",
    )
    .bind(&f.owner)
    .bind(&request.invocation_id)
    .fetch_one(f.d.store.pool())
    .await
    .expect("rollback");
    assert_eq!(count, 0);
    let job =
        f.d.store
            .submit_for_user(&f.actor, &f.owner, "submit-after-failed-barrier", &request)
            .await
            .expect("no accepted withdrawal existed");
    f.d.store
        .cancel_for_user(&f.actor, &f.owner, &job.job_id, "retire")
        .await
        .expect("retire only test Job");
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn organization_metadata_membership_cannot_fence_an_unattributed_quote() {
    let f = setup_model_scope(
        "client-dr",
        &"b".repeat(64),
        &"c".repeat(64),
        "withdrawal organization fixture".into(),
        32,
        true,
    )
    .await;
    let request = pending(&f).await;
    assert_eq!(
        withdraw(&f, &request, "no-creator-proof", f.headers.clone())
            .await
            .0,
        403,
        "a budget/quote in the same organization is not original-user proof"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invocation_withdrawals WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no barrier");
    assert_eq!(count, 0);
    f.objects.drain().await.expect("drain");
}
#[tokio::test]
async fn expiry_before_barrier_commit_rolls_back_the_entire_stop() {
    let f = setup("client-ds").await;
    let request = pending(&f).await;
    let invocation = Uuid::parse_str(&request.invocation_id).expect("fixture ID");
    let suffix = Uuid::new_v4().simple().to_string();
    sqlx::query("UPDATE user_sessions SET idle_expires_at=floor(extract(epoch FROM clock_timestamp()))::bigint+2,absolute_expires_at=floor(extract(epoch FROM clock_timestamp()))::bigint+2 WHERE user_id=$1").bind(&f.user).execute(f.d.store.pool()).await.expect("bounded fixture expiry");
    let ddl = format!(
        "CREATE FUNCTION withdrawal_expiry_{suffix}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.invocation_id='{invocation}' THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE TRIGGER withdrawal_expiry_{suffix} BEFORE INSERT ON invocation_withdrawals FOR EACH ROW EXECUTE FUNCTION withdrawal_expiry_{suffix}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(f.d.store.pool())
        .await
        .expect("scoped delay");
    let result = withdraw(&f, &request, "expiry", f.headers.clone()).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER withdrawal_expiry_{suffix} ON invocation_withdrawals; DROP FUNCTION withdrawal_expiry_{suffix}();"))).execute(f.d.store.pool()).await.expect("remove exact delay");
    assert_eq!(result.0, 403);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invocation_withdrawals WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no expired acceptance");
    assert_eq!(count, 0);
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn submit_snapshot_started_before_waited_barrier_cannot_miss_the_barrier() {
    let f = setup("client-dt").await;
    let request = pending(&f).await;
    let lock = request_digest(
        "user.invocation.lock.v1",
        &(&f.owner, &request.invocation_id),
    )
    .expect("same lock identity");
    let mut guard = f.d.store.pool().begin().await.expect("test lock");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(lock)
        .execute(&mut *guard)
        .await
        .expect("block both actual operations");
    let tag = held_tag(&mut guard).await;
    // A waiter for another Invocation must not satisfy our ordering witness.
    // The full control suite runs independent barrier tests concurrently.
    let unrelated_key = format!("unrelated-withdrawal-fixture-{}", Uuid::new_v4());
    let mut unrelated_guard = f.d.store.pool().begin().await.expect("unrelated guard");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&unrelated_key)
        .execute(&mut *unrelated_guard)
        .await
        .expect("unrelated lock");
    let unrelated_tag = held_tag(&mut unrelated_guard).await;
    assert_ne!(tag, unrelated_tag);
    let unrelated_pool = f.d.store.pool().clone();
    let unrelated_waiter = tokio::spawn(async move {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(unrelated_key)
            .execute(&unrelated_pool)
            .await
            .expect("unrelated waiter");
    });
    waiters(f.d.store.pool(), unrelated_tag, 1).await;
    assert_eq!(waiter_count(f.d.store.pool(), tag).await, 0);
    let store = f.d.store.clone();
    let actor = f.actor.clone();
    let owner = f.owner.clone();
    let stop_request = request.clone();
    let stopping = tokio::spawn(async move {
        store
            .withdraw_invocation(&actor, &owner, "ordered-stop", &stop_request)
            .await
    });
    waiters(f.d.store.pool(), tag, 1).await;
    let store = f.d.store.clone();
    let actor = f.actor.clone();
    let owner = f.owner.clone();
    let submitting = tokio::spawn(async move {
        store
            .submit_for_user(&actor, &owner, "old-snapshot-submit", &request)
            .await
    });
    waiters(f.d.store.pool(), tag, 2).await;
    guard
        .commit()
        .await
        .expect("withdraw first, submit retains an older snapshot");
    assert!(
        stopping
            .await
            .expect("stop task")
            .expect("stop receipt")
            .prevented
    );
    assert!(matches!(
        submitting.await.expect("submit task"),
        Err(StoreError::InvocationWithdrawn)
    ));
    let balance: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("no hidden reservation");
    assert_eq!(balance, (1000, 0));
    unrelated_guard
        .commit()
        .await
        .expect("release only fixture decoy");
    unrelated_waiter.await.expect("decoy completed");
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn two_waiting_identical_withdrawals_return_the_same_original_receipt() {
    let f = setup("client-du").await;
    let request = pending(&f).await;
    let lock = request_digest(
        "user.invocation.lock.v1",
        &(&f.owner, &request.invocation_id),
    )
    .expect("lock");
    let mut guard = f.d.store.pool().begin().await.expect("guard");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(lock)
        .execute(&mut *guard)
        .await
        .expect("hold");
    let tag = held_tag(&mut guard).await;
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let store = f.d.store.clone();
        let actor = f.actor.clone();
        let owner = f.owner.clone();
        let request = request.clone();
        tasks.push(tokio::spawn(async move {
            store
                .withdraw_invocation(&actor, &owner, "same-stop", &request)
                .await
        }));
    }
    waiters(f.d.store.pool(), tag, 2).await;
    guard.commit().await.expect("release");
    let first = tasks
        .remove(0)
        .await
        .expect("first task")
        .expect("first receipt");
    let second = tasks
        .remove(0)
        .await
        .expect("second task")
        .expect("same receipt");
    assert_eq!(first.receipt_id, second.receipt_id);
    assert!(first.prevented && second.prevented);
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM durable_events e JOIN event_streams s USING(stream_id) WHERE s.owner_id=$1 AND s.entity_id=$2 AND e.event_type='invocation.withdrawn_before_acceptance'").bind(&f.owner).bind(&request.invocation_id).fetch_one(f.d.store.pool()).await.expect("one event");
    assert_eq!(events, 1);
    f.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn original_organization_requester_can_stop_after_spend_revocation_but_other_owner_cannot() {
    let f = setup_model_scope(
        "client-dv",
        &"b".repeat(64),
        &"c".repeat(64),
        "organization original-user identity fixture".into(),
        32,
        true,
    )
    .await;
    let other = setup("client-dw").await;
    let request = pending(&f).await;
    let mut input = f.request.clone();
    input.invocation_id = request.invocation_id.clone();
    let bytes = serde_json::to_vec(&input).expect("input");
    assert_eq!(
        request_digest("model-request-v1", &input).expect("digest"),
        request.request_projection_digest
    );
    let upload = CreateUpload {
        upload_id: Uuid::new_v4().to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/json".into(),
    };
    f.objects
        .begin_upload(&f.actor, &f.owner, &upload)
        .await
        .expect("input metadata");
    let artifact = f
        .objects
        .put(
            f.actor.clone(),
            f.owner.clone(),
            upload.upload_id.clone(),
            tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes))]),
        )
        .await
        .expect("input bytes");
    let meter = request_digest("withdraw.identity.fixture", &f.owner).expect("fixture digest");
    let price: String = sqlx::query_scalar("SELECT price_version FROM quotes WHERE quote_id=$1")
        .bind(&request.quote_id)
        .fetch_one(f.d.store.pool())
        .await
        .expect("original price ID");
    // Explicit metadata/one-unit fixture for original-requester authorization;
    // this is not actual tokenizer or price-approval evidence.
    sqlx::query("INSERT INTO trusted_meter_profiles(profile_digest,descriptor) VALUES($1,$2)").bind(&meter).bind(serde_json::json!({"fixture":"withdrawal identity only","execution_digest":f.execution_digest})).execute(f.d.store.pool()).await.expect("metadata fixture");
    sqlx::query("INSERT INTO text_prices(price_version,node_id,execution_profile_id,meter_profile_digest,input_price,output_price,output_limit,fee_bps) VALUES($1,$2,$3,$4,100000,0,32,0)").bind(&price).bind(&f.d.node).bind(&f.profile).bind(&meter).execute(f.d.store.pool()).await.expect("fixture price, no publication");
    sqlx::query("INSERT INTO quote_metering(quote_id,request_id,owner_id,requester_id,request_digest,input_artifact_id,input_version_id,input_units,output_limit,price_version,meter_profile_digest,provider_owner_id,input_price,output_price,fee_bps) VALUES($1,$2,$3,$4,$5,$6,$7,1,32,$8,$9,$10,100000,0,0)").bind(&request.quote_id).bind(Uuid::new_v4().to_string()).bind(&f.owner).bind(&f.user).bind(request_digest("quote.fixture",&request).expect("digest")).bind(&artifact.artifact_id).bind(&artifact.version_id).bind(&price).bind(&meter).bind(&f.d.owner).execute(f.d.store.pool()).await.expect("original requester fact fixture");
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'owner')")
        .bind(&f.owner)
        .bind(&other.user)
        .execute(f.d.store.pool())
        .await
        .expect("another owner");
    assert_eq!(
        withdraw(&f, &request, "foreign-owner", other.headers.clone())
            .await
            .0,
        403
    );
    sqlx::query("UPDATE memberships SET role='viewer' WHERE organization_id=$1 AND user_id=$2")
        .bind(&f.owner)
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("original user read-only");
    sqlx::query(
        "UPDATE budget_user_grants SET can_submit=false WHERE root_budget_id=$1 AND user_id=$2",
    )
    .bind(&request.root_budget_id)
    .bind(&f.user)
    .execute(f.d.store.pool())
    .await
    .expect("new spending revoked");
    let stop = withdraw(&f, &request, "own-stop", f.headers.clone()).await;
    assert_eq!(stop.0, 200);
    assert_eq!(stop.1["prevented"], true);
    sqlx::query("UPDATE memberships SET role='member' WHERE organization_id=$1 AND user_id=$2")
        .bind(&f.owner)
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("explicit permission restoration fixture");
    sqlx::query(
        "UPDATE budget_user_grants SET can_submit=true WHERE root_budget_id=$1 AND user_id=$2",
    )
    .bind(&request.root_budget_id)
    .bind(&f.user)
    .execute(f.d.store.pool())
    .await
    .expect("grant restored");
    assert!(matches!(
        f.d.store
            .submit_for_user(&f.actor, &f.owner, "late-org-submit", &request)
            .await,
        Err(StoreError::InvocationWithdrawn)
    ));
    f.objects.drain().await.expect("drain");
    other.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn running_original_withdrawal_retains_hold_until_separate_cpu_cleanup() {
    let (f, server, connection, cleanup) =
        super::cleanup_cases::setup_cleanup("client-dx", true).await;
    let job =
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("original running Job");
    let quote: String = sqlx::query_scalar("SELECT quote_id FROM jobs WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("quote");
    let request = SubmitJob {
        root_budget_id: job.root_budget_id,
        invocation_id: f.request.invocation_id.clone(),
        purpose: InvocationPurpose::Primary,
        quote_id: quote,
        request_projection_digest: request_digest("model-request-v1", &f.request)
            .expect("projection"),
    };
    let response = withdraw(&f, &request, "running-withdraw", f.headers.clone()).await;
    assert_eq!(response.0, 200);
    assert_eq!(response.1["prevented"], false);
    assert_eq!(response.1["job"]["execution_state"], "cancelling");
    assert_eq!(response.1["job"]["settlement_state"], "pending");
    assert!(response.1["job"]["charge"].is_null());
    let held: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&f.owner)
            .fetch_one(f.d.store.pool())
            .await
            .expect("held");
    assert_eq!(held, (900, 100));
    let mut raw = server
        .client(Some(&f.d.certificate))
        .await
        .expect("actual mTLS");
    raw.confirm_cpu_cleanup(cleanup)
        .await
        .expect("separate declared CPU cleanup fixture");
    let terminal = withdraw(&f, &request, "running-withdraw", f.headers.clone()).await;
    assert_eq!(terminal.1["job"]["execution_state"], "cancelled");
    assert_eq!(terminal.1["job"]["charge"], "0");
    connection.close();
    server.stop().await;
    f.objects.drain().await.expect("drain");
}
