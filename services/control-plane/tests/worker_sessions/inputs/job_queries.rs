//! HTTP/session/PG metadata recovery. No model or output-content authority.
use super::*;
async fn list(
    f: &InputFixture,
    headers: axum::http::HeaderMap,
    query: &str,
) -> (u16, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut request = axum::http::Request::builder()
        .uri(format!("/api/v1/owners/{}/jobs{query}", f.owner))
        .body(axum::body::Body::empty())
        .expect("GET");
    *request.headers_mut() = headers;
    let response = app.oneshot(request).await.expect("route");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}
async fn lookup(
    f: &InputFixture,
    headers: axum::http::HeaderMap,
    invocation: &str,
) -> (u16, serde_json::Value) {
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let mut request = axum::http::Request::builder()
        .uri(format!(
            "/api/v1/owners/{}/jobs/by-invocation/{invocation}",
            f.owner
        ))
        .body(axum::body::Body::empty())
        .expect("GET");
    *request.headers_mut() = headers;
    let response = app.oneshot(request).await.expect("lookup");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}
#[tokio::test]
async fn job_metadata_lists_only_current_submitter_without_restoring_deleted_content() {
    let f = setup_model_scope(
        "client-di",
        &"b".repeat(64),
        &"c".repeat(64),
        "job metadata query".into(),
        32,
        true,
    )
    .await;
    let other = setup("client-dj").await;
    assert_eq!(
        list(&f, other.headers.clone(), "").await.0,
        403,
        "no membership"
    );
    sqlx::query("INSERT INTO memberships(organization_id,user_id,role) VALUES($1,$2,'admin')")
        .bind(&f.owner)
        .bind(&other.user)
        .execute(f.d.store.pool())
        .await
        .expect("other team admin");
    let empty = list(&f, other.headers.clone(), "").await;
    assert_eq!(empty.0, 200);
    assert_eq!(empty.1["data"], serde_json::json!([]));
    let original = lookup(&f, f.headers.clone(), &f.request.invocation_id).await;
    assert_eq!(original.0, 200);
    assert_eq!(original.1["job"]["job_id"], f.job);
    assert_eq!(original.1["user_id"], f.user);
    assert_eq!(original.1["owner_id"], f.owner);
    let quote: String = sqlx::query_scalar("SELECT quote_id FROM jobs WHERE job_id=$1")
        .bind(&f.job)
        .fetch_one(f.d.store.pool())
        .await
        .expect("original quote");
    assert_eq!(original.1["quote_id"], quote);
    assert!(original.1.get("projection_digest").is_none());
    assert_eq!(
        lookup(&f, other.headers.clone(), &f.request.invocation_id)
            .await
            .0,
        404,
        "another submitter cannot recover the original Job"
    );
    assert_eq!(
        lookup(&f, f.headers.clone(), "missing-invocation").await.0,
        404
    );
    assert_eq!(
        lookup(
            &f,
            f.headers.clone(),
            &format!("{}?user_id=other", f.request.invocation_id)
        )
        .await
        .0,
        422
    );
    let effects:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM job_inputs WHERE job_id=$1),(SELECT count(*) FROM worker_execution_attempts WHERE job_id=$1)").bind(&f.job).fetch_one(f.d.store.pool()).await.expect("no effects");
    assert_eq!(effects, (0, 0));
    let own = list(&f, f.headers.clone(), "?limit=1").await;
    assert_eq!(own.0, 200);
    assert_eq!(own.1["owner_id"], f.owner);
    assert_eq!(own.1["user_id"], f.user);
    assert_eq!(own.1["data"].as_array().expect("rows").len(), 1);
    assert_eq!(own.1["data"][0]["job_id"], f.job);
    assert!(own.1["next_cursor"].is_null());
    assert!(own.1["data"][0].get("prompt").is_none());
    assert_eq!(
        list(&f, f.headers.clone(), &format!("?after={}", f.job))
            .await
            .1["data"],
        serde_json::json!([])
    );
    for query in ["?limit=101", "?limit=0", "?submitter_id=another"] {
        assert_eq!(list(&f, f.headers.clone(), query).await.0, 422);
    }
    let budget =
        f.d.store
            .job(&f.owner, &f.job)
            .await
            .expect("original root")
            .root_budget_id;
    let mut expected = vec![f.job.clone()];
    for _ in 0..2 {
        let quote = Uuid::new_v4().to_string();
        let mut input = f.request.clone();
        input.invocation_id = Uuid::new_v4().to_string();
        let projection = request_digest("model-request-v1", &input).expect("projection");
        sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'query-fixture',100,clock_timestamp()+interval '1 hour')").bind(&quote).bind(&f.owner).bind(&budget).bind(&input.invocation_id).bind(&projection).execute(f.d.store.pool()).await.expect("declared quote fixture, not a real price approval");
        let request = SubmitJob {
            root_budget_id: budget.clone(),
            invocation_id: input.invocation_id,
            purpose: InvocationPurpose::Primary,
            quote_id: quote,
            request_projection_digest: projection,
        };
        let job =
            f.d.store
                .submit_for_user(&f.actor, &f.owner, &Uuid::new_v4().to_string(), &request)
                .await
                .expect("actual authorized reservation, no input/dispatch yet");
        expected.push(job.job_id);
    }
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let query = cursor.as_ref().map_or_else(
            || "?limit=1".to_owned(),
            |id| format!("?limit=1&after={id}"),
        );
        let page = list(&f, f.headers.clone(), &query).await;
        assert_eq!(page.0, 200);
        let data = page.1["data"].as_array().expect("page");
        assert_eq!(data.len(), 1);
        seen.push(data[0]["job_id"].as_str().expect("ID").to_owned());
        cursor = page.1["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
        assert!(seen.len() < 4, "cursor must advance");
    }
    expected.sort();
    assert_eq!(
        seen, expected,
        "all own Jobs exactly once in stable cursor order"
    );
    f.objects
        .delete(&f.actor, &f.owner, &f.upload.upload_id)
        .await
        .expect("delete source");
    assert!(
        list(&f, f.headers.clone(), "").await.1["data"]
            .as_array()
            .expect("metadata")
            .iter()
            .any(|job| job["job_id"] == f.job),
        "metadata remains a fact, not content permission"
    );
    assert!(
        f.objects
            .read(&f.actor, &f.owner, &f.upload.upload_id, &f.version)
            .await
            .is_err()
    );
    assert_eq!(
        lookup(&f, f.headers.clone(), &f.request.invocation_id)
            .await
            .1["job"]["job_id"],
        f.job,
        "deleted input does not disappear from metadata, nor regrant content"
    );
    for job in &seen {
        f.d.store
            .cancel_for_user(&f.actor, &f.owner, job, &format!("retire-{job}"))
            .await
            .expect("retire only never-dispatched test Jobs");
    }
    sqlx::query("UPDATE user_sessions SET revoked=true WHERE user_id=$1")
        .bind(&f.user)
        .execute(f.d.store.pool())
        .await
        .expect("revoke");
    assert_eq!(list(&f, f.headers.clone(), "").await.0, 401);
    f.objects.drain().await.expect("drain");
    other.objects.drain().await.expect("drain");
}

#[tokio::test]
async fn membership_changes_and_owner_restriction_preserve_original_job_and_ledger_identity() {
    use orange_control_plane::team::TeamStore;
    use orange_protocol::team::{ChangeMembership, TeamRole};
    let f = setup_model_scope(
        "client-eg",
        &"c".repeat(64),
        &"d".repeat(64),
        "owner lifecycle".into(),
        32,
        true,
    )
    .await;
    let coowner = setup("client-eh").await;
    let pool = f.d.store.pool();
    let team = TeamStore::new(pool.clone());
    let version: i64 =
        sqlx::query_scalar("SELECT policy_version FROM organizations WHERE organization_id=$1")
            .bind(&f.owner)
            .fetch_one(pool)
            .await
            .expect("current org");
    let v = team
        .set_membership(
            &f.user,
            &f.owner,
            "add-second-owner",
            &ChangeMembership {
                user_id: coowner.user.clone(),
                role: Some(TeamRole::Owner),
                expected_policy_version: version,
            },
        )
        .await
        .expect("second owner");
    let history = || {
        sqlx::query_as::<_,(String,String,String,String,String,i64)>("SELECT j.owner_id,q.owner_id,b.owner_id,l.owner_id,l.transaction_id,j.reserved FROM jobs j JOIN quotes q USING(quote_id) JOIN budget_roots b ON b.root_budget_id=j.root_budget_id JOIN ledger_transactions l ON l.business_key='reserve:'||j.job_id WHERE j.job_id=$1").bind(&f.job).fetch_one(pool)
    };
    let original = history().await.expect("actual accepted reservation");
    assert_eq!(
        (&original.0, &original.1, &original.2, &original.3),
        (&f.owner, &f.owner, &f.owner, &f.owner)
    );
    assert_eq!(
        lookup(&f, f.headers.clone(), &f.request.invocation_id)
            .await
            .0,
        200
    );
    assert!(
        f.objects
            .read(&f.actor, &f.owner, &f.upload.upload_id, &f.version)
            .await
            .is_ok()
    );
    let downgraded = team
        .set_membership(
            &coowner.user,
            &f.owner,
            "downgrade-original",
            &ChangeMembership {
                user_id: f.user.clone(),
                role: Some(TeamRole::Billing),
                expected_policy_version: v.policy_version,
            },
        )
        .await
        .expect("current owner downgraded");
    assert!(
        f.objects
            .read(&f.actor, &f.owner, &f.upload.upload_id, &f.version)
            .await
            .is_err()
    );
    let removed = team
        .set_membership(
            &coowner.user,
            &f.owner,
            "remove-original",
            &ChangeMembership {
                user_id: f.user.clone(),
                role: None,
                expected_policy_version: downgraded.policy_version,
            },
        )
        .await
        .expect("remove member");
    assert!(removed.role.is_none());
    assert_eq!(
        lookup(&f, f.headers.clone(), &f.request.invocation_id)
            .await
            .0,
        403
    );
    assert_eq!(
        lookup(&f, coowner.headers.clone(), &f.request.invocation_id)
            .await
            .0,
        404,
        "team owner cannot take over the former submitter's job"
    );
    assert_eq!(history().await.expect("retained history"), original);
    sqlx::query("UPDATE owners SET state='restricted' WHERE owner_id=$1")
        .bind(&f.owner)
        .execute(pool)
        .await
        .expect("explicit organization restriction fixture");
    assert_eq!(list(&f, coowner.headers.clone(), "").await.0, 403);
    assert_eq!(
        history()
            .await
            .expect("restriction preserves ownership and reservation"),
        original
    );
    f.objects.drain().await.expect("objects");
    coowner.objects.drain().await.expect("objects");
}

#[tokio::test]
async fn typed_job_status_requires_the_original_owner_job_and_authenticated_submitter() {
    let f = setup("client-ay").await;
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    async fn status(
        app: axum::Router,
        headers: &axum::http::HeaderMap,
        owner: &str,
        job: &str,
    ) -> (u16, serde_json::Value) {
        let mut request = axum::http::Request::builder()
            .uri(format!("/api/v1/owners/{owner}/jobs/{job}"))
            .body(axum::body::Body::empty())
            .expect("request");
        *request.headers_mut() = headers.clone();
        let response = app.oneshot(request).await.expect("HTTP route");
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("wire JSON"))
    }
    let (ok, view) = status(app.clone(), &f.headers, &f.owner, &f.job).await;
    assert_eq!(ok, 200);
    assert_eq!(view["job_id"], f.job);
    let (missing, body) = status(app.clone(), &f.headers, &f.owner, &f.user).await;
    assert_eq!(missing, 404);
    assert!(body.get("job_id").is_none());
    let (invalid, body) = status(app.clone(), &f.headers, &f.owner, "bad%20job").await;
    assert_eq!(invalid, 400);
    assert!(body.get("job_id").is_none());
    assert_eq!(
        status(app, &Default::default(), &f.owner, &f.job).await.0,
        401
    );
}
