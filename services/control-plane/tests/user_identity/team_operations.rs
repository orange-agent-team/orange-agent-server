use super::*;

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
    code: u16,
) -> Value {
    let mut h = headers(f);
    h.insert("idempotency-key", key.parse().expect("key"));
    let (status, v) = request(f, method, path, body, h).await;
    assert_eq!(status.as_u16(), code, "{method} {path} {v}");
    v
}
async fn org(f: &Fixture, cap: u16) -> String {
    call(
        f,
        "POST",
        "/api/v1/organizations",
        &Uuid::new_v4().to_string(),
        Some(json!({"name":"Team journey","member_limit":cap})),
        200,
    )
    .await["organization_id"]
        .as_str()
        .expect("org")
        .into()
}
async fn version(f: &Fixture, id: &str) -> String {
    call(
        f,
        "GET",
        &format!("/api/v1/organizations/{id}"),
        "read",
        None,
        200,
    )
    .await["policy_version"]
        .as_str()
        .expect("version")
        .into()
}
async fn invite(f: &Fixture, org: &str, to: &Fixture, role: &str, ttl: u32, status: u16) -> String {
    let id = Uuid::new_v4().to_string();
    let v = version(f, org).await;
    let req = json!({"invitation_id":id,"recipient_id":to.user,"role":role,"ttl_seconds":ttl,"expected_policy_version":v});
    let path = format!("/api/v1/organizations/{org}/invitations");
    let ans = call(f, "POST", &path, &id, Some(req.clone()), status).await;
    if status == 200 {
        assert_eq!(ans["state"], "pending");
        assert_eq!(call(f, "POST", &path, &id, Some(req), 200).await, ans);
    }
    id
}
async fn change(
    f: &Fixture,
    org: &str,
    target: &Fixture,
    role: Option<&str>,
    status: u16,
) -> Value {
    let v = version(f, org).await;
    call(
        f,
        "POST",
        &format!("/api/v1/organizations/{org}/members"),
        &Uuid::new_v4().to_string(),
        Some(json!({"user_id":target.user,"role":role,"expected_policy_version":v})),
        status,
    )
    .await
}
async fn accept(f: &Fixture, id: &str, code: u16) -> Value {
    call(
        f,
        "POST",
        &format!("/api/v1/team-invitations/{id}/accept"),
        id,
        Some(json!({"accept":true})),
        code,
    )
    .await
}
#[tokio::test]
async fn addressed_invites_recheck_issuer_recipient_role_and_never_restore_withdrawn_grants() {
    let owner = fixture().await;
    let store = Store::from_pool(owner.pool.clone());
    let manager = fixture_from_store(&store).await;
    let target = fixture_from_store(&store).await;
    let team = org(&owner, 3).await;
    let members = format!("/api/v1/organizations/{team}/members");
    let first = invite(&owner, &team, &manager, "admin", 600, 200).await;
    let inbox = call(
        &manager,
        "GET",
        "/api/v1/me/team-invitations",
        "read",
        None,
        200,
    )
    .await;
    assert_eq!(inbox["data"][0]["invitation_id"], first);
    assert!(inbox["data"][0]["accepted_policy_version"].is_null());
    assert!(
        call(
            &target,
            "GET",
            "/api/v1/me/team-invitations",
            "read",
            None,
            200
        )
        .await["data"]
            .as_array()
            .expect("inbox")
            .is_empty()
    );
    accept(&target, &first, 404).await;
    call(
        &manager,
        "POST",
        &format!("/api/v1/team-invitations/{first}/accept"),
        &first,
        Some(json!({"accept":false})),
        422,
    )
    .await;
    let (a, b) = tokio::join!(accept(&manager, &first, 200), accept(&manager, &first, 200));
    assert_eq!(a, b);
    assert_eq!(a["accepted_policy_version"], "2");
    assert_eq!(a["role"], "admin");
    let active = call(&owner, "GET", &members, "read", None, 200).await;
    assert_eq!(active["data"].as_array().expect("members").len(), 2);
    invite(&manager, &team, &target, "owner", 600, 403).await;
    let revoked = invite(&manager, &team, &target, "member", 600, 200).await;
    change(&owner, &team, &manager, Some("member"), 200).await;
    change(&owner, &team, &manager, Some("admin"), 200).await;
    accept(&target, &revoked, 409).await;
    let declined = invite(&owner, &team, &target, "member", 600, 200).await;
    call(
        &target,
        "POST",
        &format!("/api/v1/team-invitations/{declined}/decline"),
        &declined,
        Some(json!({})),
        200,
    )
    .await;
    accept(&target, &declined, 409).await;
    let withdrawn = invite(&owner, &team, &target, "member", 600, 200).await;
    call(
        &owner,
        "DELETE",
        &format!("/api/v1/organizations/{team}/invitations/{withdrawn}"),
        &withdrawn,
        None,
        200,
    )
    .await;
    accept(&target, &withdrawn, 409).await;
    let last = invite(&owner, &team, &target, "member", 600, 200).await;
    accept(&target, &last, 200).await;
    call(&target, "GET", &members, "read", None, 403).await;
    let budget = store
        .create_budget(
            &team,
            "member-root",
            &orange_protocol::CreateBudget {
                scope_id: "former-member".into(),
                limit: "100".to_owned().try_into().expect("limit"),
            },
        )
        .await
        .expect("root");
    sqlx::query(
        "INSERT INTO budget_user_grants(root_budget_id,user_id,can_submit) VALUES($1,$2,true)",
    )
    .bind(&budget.root_budget_id)
    .bind(&target.user)
    .execute(&owner.pool)
    .await
    .expect("old budget grant fixture");
    let v = version(&owner, &team).await;
    call(
        &owner,
        "POST",
        &members,
        "missing-role",
        Some(json!({"user_id":target.user,"expected_policy_version":v})),
        422,
    )
    .await;
    change(&owner, &team, &target, None, 200).await;
    change(&owner, &team, &target, Some("member"), 404).await;
    let new = invite(&owner, &team, &target, "member", 600, 200).await;
    accept(&target, &new, 200).await;
    let grant: bool = sqlx::query_scalar(
        "SELECT can_submit FROM budget_user_grants WHERE root_budget_id=$1 AND user_id=$2",
    )
    .bind(&budget.root_budget_id)
    .bind(&target.user)
    .fetch_one(&owner.pool)
    .await
    .expect("old grant");
    assert!(!grant);
    change(&owner, &team, &owner, None, 409).await;
    let accepted:i64=sqlx::query_scalar("SELECT count(*) FROM team_audit WHERE organization_id=$1 AND kind='invitation.accepted' AND target_id=$2").bind(team).bind(first).fetch_one(&owner.pool).await.expect("one acceptance");
    assert_eq!(accepted, 1);
}
#[tokio::test]
async fn competing_acceptances_respect_member_limit_and_expiry() {
    let owner = fixture().await;
    let store = Store::from_pool(owner.pool.clone());
    let a = fixture_from_store(&store).await;
    let b = fixture_from_store(&store).await;
    let team = org(&owner, 2).await;
    let first = invite(&owner, &team, &a, "member", 1, 200).await;
    sqlx::query("SELECT pg_sleep(1.02)")
        .execute(&owner.pool)
        .await
        .expect("actual expiry");
    accept(&a, &first, 409).await;
    let a_id = invite(&owner, &team, &a, "member", 600, 200).await;
    let b_id = invite(&owner, &team, &b, "viewer", 600, 200).await;
    let race = |f: &Fixture, id: String| {
        let h = {
            let mut h = headers(f);
            h.insert("idempotency-key", id.parse().expect("key"));
            h
        };
        let path = format!("/api/v1/team-invitations/{id}/accept");
        let router = f.router.clone();
        async move {
            let mut req = Request::builder().uri(path).method("POST");
            *req.headers_mut().expect("headers") = h;
            router
                .oneshot(
                    req.header("content-type", "application/json")
                        .body(Body::from(r#"{"accept":true}"#))
                        .expect("request"),
                )
                .await
                .expect("router")
                .status()
        }
    };
    let (x, y) = tokio::join!(race(&a, a_id), race(&b, b_id));
    assert!(
        (x == StatusCode::OK && y == StatusCode::CONFLICT)
            || (x == StatusCode::CONFLICT && y == StatusCode::OK)
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE organization_id=$1 AND active")
            .bind(&team)
            .fetch_one(&owner.pool)
            .await
            .expect("cap");
    assert_eq!(count, 2);
}

#[tokio::test]
async fn team_hard_limit_applies_to_actual_member_jobs_and_never_copies_the_organization_wallet() {
    use std::os::unix::fs::PermissionsExt;
    let owner = fixture().await;
    let db = Store::from_pool(owner.pool.clone());
    let mut member = fixture_from_store(&db).await;
    let finance = fixture_from_store(&db).await;
    let other = fixture_from_store(&db).await;
    let team = org(&owner, 5).await;
    for (f, role) in [
        (&member, "member"),
        (&finance, "billing"),
        (&other, "admin"),
    ] {
        let i = invite(&owner, &team, f, role, 300, 200).await;
        accept(f, &i, 200).await;
    }
    let policy = format!("/api/v1/organizations/{team}/budget-policy");
    let budgets = format!("/api/v1/organizations/{team}/budgets");
    let old = call(&owner, "GET", &policy, "read", None, 200).await;
    assert!(old["hard_limit"].is_null());
    let p = json!({"expected_version":"0","owner_ceiling":"1000","hard_limit":"900"});
    call(&finance, "POST", &policy, "no-raise", Some(p.clone()), 403).await;
    let active = call(&owner, "POST", &policy, "cap", Some(p.clone()), 200).await;
    assert_eq!(active["version"], "1");
    assert_eq!(
        active,
        call(&owner, "POST", &policy, "cap", Some(p), 200).await
    );
    call(
        &other,
        "POST",
        &policy,
        "exceed-ceiling",
        Some(json!({"expected_version":"1","hard_limit":"1100"})),
        409,
    )
    .await;
    call(
        &other,
        "POST",
        &policy,
        "change-owner-ceiling",
        Some(json!({"expected_version":"1","owner_ceiling":"2000","hard_limit":"900"})),
        403,
    )
    .await;
    call(&member, "GET", &policy, "read", None, 403).await;
    assert_eq!(
        call(&finance, "GET", &policy, "read", None, 200).await["hard_limit"],
        "900"
    );
    let a = Uuid::new_v4().to_string();
    let b = Uuid::new_v4().to_string();
    let allocate = |id: &str| json!({"budget_id":id,"member_id":member.user,"limit":"700","expected_policy_version":"1"});
    let submit = |id: String| {
        let mut h = headers(&owner);
        h.insert("idempotency-key", id.parse().expect("key"));
        let value = allocate(&id);
        let f = &owner;
        let url = &budgets;
        async move { request(f, "POST", url, Some(value), h).await }
    };
    let (x, y) = tokio::join!(submit(a.clone()), submit(b.clone()));
    let root = if x.0 == StatusCode::OK {
        assert_eq!(y.0, StatusCode::CONFLICT, "{:?}", y.1);
        a
    } else {
        assert_eq!(x.0, StatusCode::CONFLICT, "{:?}", x.1);
        assert_eq!(y.0, StatusCode::OK, "{:?}", y.1);
        b
    };
    assert_eq!(
        call(&owner, "GET", &policy, "read", None, 200).await["allocated"],
        "700"
    );
    let own = call(&member, "GET", &budgets, "read", None, 200).await;
    assert_eq!(own["data"].as_array().expect("assigned roots").len(), 1);
    assert_eq!(own["data"][0]["root_budget_id"], root);
    assert_eq!(
        call(
            &owner,
            "GET",
            &format!("{budgets}/{root}"),
            "read",
            None,
            200
        )
        .await["organization_id"],
        team
    );
    let credit: i64 =
        sqlx::query_scalar("SELECT gift+purchased+earned FROM wallets WHERE owner_id=$1")
            .bind(&team)
            .fetch_one(db.pool())
            .await
            .expect("wallet");
    assert_eq!(
        credit, 0,
        "allocating 700 microcredits has not created a balance"
    );
    sqlx::query("UPDATE wallets SET gift=1200 WHERE owner_id=$1")
        .bind(&team)
        .execute(db.pool())
        .await
        .expect("isolated organizational funding");
    let private = tempfile::tempdir().expect("private object root");
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    let objects = orange_control_plane::artifacts::ArtifactStore::open_local(
        db.pool().clone(),
        private.path(),
        1048576,
        50,
    )
    .await
    .expect("object API");
    member.router = member
        .router
        .clone()
        .merge(orange_control_plane::artifacts::http::router(
            member.identity.clone(),
            objects.clone(),
        ));
    let quote = Uuid::new_v4().to_string();
    let invocation = Uuid::new_v4().to_string();
    let digest = "b".repeat(64);
    sqlx::query("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES($1,$2,$3,$4,$5,'team-quote-fixture',400,clock_timestamp()+interval '5 minutes')")
        .bind(&quote).bind(&team).bind(&root).bind(&invocation).bind(&digest).execute(db.pool()).await.expect("trusted quote fixture");
    let payload = json!({"root_budget_id":root,"invocation_id":invocation,"quote_id":quote,"purpose":"primary","request_projection_digest":digest});
    let job = call(
        &member,
        "POST",
        &format!("/api/v1/owners/{team}/jobs"),
        "start-member-job",
        Some(payload.clone()),
        200,
    )
    .await;
    assert_eq!(job["reserved"], "400");
    assert_eq!(
        call(
            &owner,
            "GET",
            &format!("{budgets}/{root}"),
            "read",
            None,
            200
        )
        .await["reserved"],
        "400"
    );
    call(
        &member,
        "POST",
        &format!("/api/v1/owners/{}/jobs", member.user),
        "wrong-wallet",
        Some(payload),
        404,
    )
    .await;
    call(
        &owner,
        "POST",
        &format!("{budgets}/{root}"),
        "too-low",
        Some(json!({"limit":"399","allow_submit":true,"expected_version":"1"})),
        409,
    )
    .await;
    let reduced = call(
        &owner,
        "POST",
        &format!("{budgets}/{root}"),
        "fit",
        Some(json!({"limit":"500","allow_submit":true,"expected_version":"1"})),
        200,
    )
    .await;
    assert_eq!(reduced["version"], "2");
    let bucket: (i64, i64) =
        sqlx::query_as("SELECT gift,reserved_gift FROM wallets WHERE owner_id=$1")
            .bind(&team)
            .fetch_one(db.pool())
            .await
            .expect("team reserve");
    assert_eq!(bucket, (800, 400));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT gift+reserved_gift+purchased FROM wallets WHERE owner_id=$1"
        )
        .bind(&member.user)
        .fetch_one(db.pool())
        .await
        .expect("personal wallet"),
        0
    );
    call(
        &member,
        "POST",
        &format!(
            "/api/v1/owners/{team}/jobs/{}/cancel",
            job["job_id"].as_str().expect("job")
        ),
        "cancel",
        Some(json!({})),
        200,
    )
    .await;
    let v = call(
        &owner,
        "POST",
        &policy,
        "lower",
        Some(json!({"expected_version":"1","hard_limit":"500"})),
        200,
    )
    .await;
    assert_eq!(v["allocated"], "500");
    change(&owner, &team, &member, None, 200).await;
    let replay = invite(&owner, &team, &member, "member", 300, 200).await;
    accept(&member, &replay, 200).await;
    let revoked = call(
        &owner,
        "GET",
        &format!("{budgets}/{root}"),
        "read",
        None,
        200,
    )
    .await;
    assert_eq!(revoked["allow_submit"], false);
    assert_eq!(revoked["version"], "3");
    call(
        &owner,
        "POST",
        &format!("{budgets}/{root}"),
        "stale-grant",
        Some(json!({"limit":"500","allow_submit":true,"expected_version":"2"})),
        409,
    )
    .await;
    assert!(
        call(&member, "GET", &budgets, "read", None, 200).await["data"]
            .as_array()
            .expect("filtered")
            .is_empty()
    );
    objects.drain().await.expect("objects closed");
}
#[tokio::test]
async fn existing_team_budget_capacity_is_reconstructed_on_upgrade_without_minting_credit() {
    use sqlx::migrate::MigrateDatabase;
    let supplied = std::env::var("ORANGE_TEAM_OLD_DATABASE").ok();
    let source = supplied.clone().unwrap_or_else(|| {
        std::env::var("ORANGE_TEST_DATABASE_URL").expect("explicit local development PostgreSQL")
    });
    let mut url = reqwest::Url::parse(&source).expect("test PostgreSQL URL");
    assert!(
        matches!(url.scheme(), "postgres" | "postgresql")
            && matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
    );
    assert!(
        url.query_pairs()
            .all(|(k, _)| matches!(k.as_ref(), "sslmode" | "connect_timeout")),
        "only local transport parameters allowed"
    );
    let owned = supplied.is_none();
    if owned {
        let name = format!("orange_team_old_{}", Uuid::new_v4().simple());
        url.set_path(&format!("/{name}"));
        sqlx::Postgres::create_database(url.as_str())
            .await
            .expect("fresh isolated team migration database");
    }
    let name = url.path().trim_start_matches('/');
    assert!(
        name.strip_prefix("orange_team_old_")
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())),
        "uniquely owned team database required"
    );
    let db = Store::connect(url.as_str()).await.expect("historical pool");
    sqlx::migrate::Migrator::with_migrations(
        sqlx::migrate!()
            .iter()
            .filter(|m| m.version <= 61)
            .cloned()
            .collect(),
    )
    .run(db.pool())
    .await
    .expect("schema 61");
    let f = fixture_from_store(&db).await;
    let team = org(&f, 4).await;
    sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount,spent,reserved,state) VALUES('old-active',$1,'active',300,50,70,'active'),('old-closed',$1,'closed',200,10,0,'closed')").bind(&team).execute(db.pool()).await.expect("historic active and closed budgets");
    db.migrate().await.expect("actual upgrade");
    db.migrate().await.expect("idempotent migration");
    let policy = format!("/api/v1/organizations/{team}/budget-policy");
    let state = call(&f, "GET", &policy, "read", None, 200).await;
    assert_eq!(state["allocated"], "310");
    assert!(state["hard_limit"].is_null());
    call(
        &f,
        "POST",
        &policy,
        "reject-lower",
        Some(json!({"expected_version":"0","owner_ceiling":"400","hard_limit":"309"})),
        409,
    )
    .await;
    call(
        &f,
        "POST",
        &policy,
        "admit",
        Some(json!({"expected_version":"0","owner_ceiling":"400","hard_limit":"310"})),
        200,
    )
    .await;
    let rejected = db
        .create_budget(
            &team,
            "legacy-bypass",
            &orange_protocol::CreateBudget {
                scope_id: "excess".into(),
                limit: "1".to_owned().try_into().expect("one"),
            },
        )
        .await;
    assert!(
        rejected.is_err(),
        "direct older creation path cannot exceed the new scoped cap"
    );
    let balance: i64 =
        sqlx::query_scalar("SELECT gift+earned+purchased FROM wallets WHERE owner_id=$1")
            .bind(team)
            .fetch_one(db.pool())
            .await
            .expect("old balance");
    assert_eq!(balance, 0);
    if owned {
        db.pool().close().await;
        // The normal drop fails if another connection still owns the test database.
        sqlx::Postgres::drop_database(url.as_str())
            .await
            .expect("retire only this test's database");
    }
}

#[tokio::test]
#[ignore = "requires the built independent Web repository and real Chrome"]
async fn real_owner_and_member_browsers_recover_an_invitation_and_scope_the_member_budget() {
    use tokio::io::AsyncWriteExt;
    let owner = fixture().await;
    let db = Store::from_pool(owner.pool.clone());
    let member = fixture_from_store(&db).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("API bind");
    let addr = listener.local_addr().expect("API port");
    let (stop, ended) = tokio::sync::oneshot::channel::<()>();
    let app = owner.router.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = ended.await;
            })
            .await
            .expect("server")
    });
    let name = format!("Team-{}", Uuid::new_v4().simple());
    let fixture = json!({"api":format!("http://{addr}"),"origin":owner.identity.config.user_origin,"name":name,
        "owner":owner.token,"member":member.token,"recipient":member.user,"screenshot":std::env::var("ORANGE_TEAM_SCREENSHOT").expect("evidence")});
    let mut child = tokio::process::Command::new(std::env::var("ORANGE_TEAM_NODE").expect("node"))
        .arg(std::env::var("ORANGE_TEAM_DRIVER").expect("Chrome driver"))
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("Chrome");
    let mut stdin = child.stdin.take().expect("fixture pipe");
    stdin
        .write_all(&serde_json::to_vec(&fixture).expect("fixture payload"))
        .await
        .expect("private session delivery");
    drop(stdin);
    let success = tokio::time::timeout(std::time::Duration::from_secs(110), child.wait())
        .await
        .expect("browser deadline")
        .expect("driver exit");
    let _ = stop.send(());
    server.await.expect("server joined");
    assert!(success.success(), "owner/member browser workflow");
    let facts:(i64,i64,bool,i64,i64)=sqlx::query_as("SELECT p.hard_limit,p.allocated::bigint,g.can_submit,w.gift+w.earned+w.purchased,
       (SELECT count(*) FROM team_audit t WHERE t.organization_id=o.organization_id AND t.kind='invitation.issued')
       FROM organizations o JOIN organization_budget_policies p USING(organization_id) JOIN member_budget_allocations a USING(organization_id) JOIN budget_user_grants g USING(root_budget_id) JOIN wallets w ON w.owner_id=o.organization_id
       WHERE o.name=$1 AND a.member_id=$2 AND g.user_id=$2").bind(name).bind(member.user).fetch_one(db.pool()).await.expect("independent PG confirmation");
    assert_eq!(facts, (700, 500, false, 0, 1));
}
