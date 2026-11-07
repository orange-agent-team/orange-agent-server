//! Real PostgreSQL and OpenDAL filesystem. User sessions are explicit fixtures.
use super::*;
use crate::store::Store;
struct Fixture {
    store: ArtifactStore,
    actor: ArtifactActor,
    owner: String,
    root: tempfile::TempDir,
}
async fn fixture() -> Fixture {
    let db =
        Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("real database required"))
            .await
            .expect("db");
    db.migrate().await.expect("migrate");
    let owner = id();
    let user = id();
    let session = id();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(db.pool())
        .await
        .expect("owner fixture");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'artifact-fixture',$1,$2)").bind(&user).bind(&owner).execute(db.pool()).await.expect("user fixture");
    sqlx::query("INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,0,floor(extract(epoch FROM clock_timestamp()))::bigint+3600,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)").bind(id()).bind(&session).bind(&user).bind(id()).execute(db.pool()).await.expect("session fixture");
    let root = tempfile::tempdir().expect("root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private");
    }
    let store = ArtifactStore::open_local(db.pool().clone(), root.path(), 1024 * 1024, 100)
        .await
        .expect("real filesystem");
    Fixture {
        store,
        actor: ArtifactActor {
            user,
            session,
            write: true,
        },
        owner,
        root,
    }
}
fn request(bytes: &[u8]) -> CreateUpload {
    CreateUpload {
        upload_id: id(),
        sha256: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len().to_string(),
        media_type: "application/octet-stream".into(),
    }
}
fn input(
    bytes: &[u8],
) -> impl Stream<Item = Result<opendal::Buffer, ArtifactError>> + Send + Unpin + 'static {
    tokio_stream::iter(vec![Ok(opendal::Buffer::from(bytes.to_vec()))])
}
#[tokio::test]
async fn immutable_upload_replay_and_exact_version_read() {
    let f = fixture().await;
    let bytes = b"private test artifact";
    let req = request(bytes);
    let created = f
        .store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    assert_eq!(created.state, "staging");
    assert!(matches!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &created.version_id)
            .await,
        Err(ArtifactError::NotReady)
    ));
    let ready = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(bytes),
        )
        .await
        .expect("real write/copy/publish");
    assert_eq!(ready.state, "available");
    assert_eq!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(bytes)
            )
            .await
            .expect("same bytes replay"),
        ready
    );
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(b"wrong bytes")
            )
            .await,
        Err(ArtifactError::Integrity)
    ));
    let (_, data, _) = f
        .store
        .read(&f.actor, &f.owner, &req.upload_id, &ready.version_id)
        .await
        .expect("read");
    assert_eq!(data.to_vec(), bytes);
    assert!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, "wrong-version")
            .await
            .is_err()
    );
    let mut changed = req.clone();
    changed.sha256 = "a".repeat(64);
    assert!(matches!(
        f.store.begin_upload(&f.actor, &f.owner, &changed).await,
        Err(ArtifactError::Conflict)
    ));
    f.store.drain().await.expect("owned tasks ended");
}
#[tokio::test]
async fn revoke_during_upload_never_publishes_the_copied_object() {
    let f = fixture().await;
    let req = request(b"abcdef");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let (send, recv) = tokio::sync::mpsc::channel(2);
    let store = f.store.clone();
    let actor = f.actor.clone();
    let owner = f.owner.clone();
    let upload = req.upload_id.clone();
    let task = tokio::spawn(async move {
        store
            .put(
                actor,
                owner,
                upload,
                tokio_stream::wrappers::ReceiverStream::new(recv),
            )
            .await
    });
    send.send(Ok(opendal::Buffer::from("abc")))
        .await
        .expect("first bytes");
    tokio::time::timeout(Duration::from_secs(5),async {loop {let count:i64=sqlx::query_scalar("SELECT count(*) FROM artifact_transfers t JOIN artifact_uploads u USING(artifact_id) WHERE u.owner_id=$1 AND u.upload_id=$2").bind(&f.owner).bind(&req.upload_id).fetch_one(&f.store.inner.pool).await.expect("writing");if count==1{break;}tokio::task::yield_now().await;}}).await.expect("prepared");
    sqlx::query("UPDATE user_sessions SET revoked=true WHERE session_id=$1")
        .bind(&f.actor.session)
        .execute(&f.store.inner.pool)
        .await
        .expect("real revoke during I/O");
    send.send(Ok(opendal::Buffer::from("def")))
        .await
        .expect("last bytes");
    drop(send);
    assert!(matches!(
        task.await.expect("task"),
        Err(ArtifactError::Forbidden)
    ));
    let state:String=sqlx::query_scalar("SELECT a.state FROM artifacts a JOIN artifact_uploads u USING(artifact_id) WHERE u.owner_id=$1 AND u.upload_id=$2").bind(&f.owner).bind(&req.upload_id).fetch_one(&f.store.inner.pool).await.expect("state");
    assert_eq!(state, "staging");
    assert!(
        f.store
            .status(&f.actor, &f.owner, &req.upload_id)
            .await
            .is_err()
    );
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn deletion_and_collection_preserve_tombstone_and_release_capacity_once() {
    let f = fixture().await;
    let req = request(b"contents");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let ready = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"contents"),
        )
        .await
        .expect("put");
    assert_eq!(f.store.collect(100).await.expect("remove staging"), 1);
    let used: i64 = sqlx::query_scalar("SELECT used_bytes FROM artifact_capacity WHERE root_id=$1")
        .bind(&f.store.inner.objects.root_id)
        .fetch_one(&f.store.inner.pool)
        .await
        .expect("capacity");
    assert_eq!(used, 8);
    f.store
        .delete(&f.actor, &f.owner, &req.upload_id)
        .await
        .expect("delete");
    assert!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &ready.version_id)
            .await
            .is_err()
    );
    assert_eq!(f.store.collect(100).await.expect("retention"), 0);
    sqlx::query("UPDATE artifacts SET deleted_at=clock_timestamp()-interval '25 hours' WHERE artifact_id=$1").bind(&ready.artifact_id).execute(&f.store.inner.pool).await.expect("time fixture");
    assert_eq!(f.store.collect(100).await.expect("physical removal"), 1);
    assert_eq!(
        f.store.collect(100).await.expect("idempotent collection"),
        0
    );
    let used: i64 = sqlx::query_scalar("SELECT used_bytes FROM artifact_capacity WHERE root_id=$1")
        .bind(&f.store.inner.objects.root_id)
        .fetch_one(&f.store.inner.pool)
        .await
        .expect("capacity");
    assert_eq!(used, 0);
    assert_eq!(
        f.store
            .begin_upload(&f.actor, &f.owner, &req)
            .await
            .expect("original ID tombstone")
            .state,
        "deleted"
    );
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn private_root_exclusive_lock_and_corruption_are_enforced() {
    let f = fixture().await;
    assert!(
        ArtifactStore::open_local(f.store.inner.pool.clone(), f.root.path(), 1024 * 1024, 100)
            .await
            .is_err()
    );
    let req = request(b"abcd");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let v = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"abcd"),
        )
        .await
        .expect("put");
    let key:String=sqlx::query_scalar("SELECT t.published_key FROM artifact_transfers t JOIN artifacts a ON a.published_transfer_id=t.transfer_id WHERE a.artifact_id=$1").bind(&v.artifact_id).fetch_one(&f.store.inner.pool).await.expect("key");
    std::fs::write(f.root.path().join(key), b"bad!").expect("corruption fixture");
    assert!(matches!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &v.version_id)
            .await,
        Err(ArtifactError::Integrity)
    ));
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(b"abcd")
            )
            .await,
        Err(ArtifactError::Integrity)
    ));
    f.store.drain().await.expect("drain");
}

#[tokio::test]
async fn partial_bad_body_is_invisible_and_orphan_quota_is_reclaimed_once() {
    let f = fixture().await;
    let req = request(b"abcdef");
    let created = f
        .store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(b"abc")
            )
            .await,
        Err(ArtifactError::Integrity)
    ));
    assert!(matches!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &created.version_id)
            .await,
        Err(ArtifactError::NotReady)
    ));
    sqlx::query("UPDATE artifact_transfers SET created_at=clock_timestamp()-interval '2 hours',expires_at=clock_timestamp()-interval '1 hour' WHERE artifact_id=$1").bind(&created.artifact_id).execute(&f.store.inner.pool).await.expect("expiry fixture");
    assert_eq!(f.store.collect(10).await.expect("collect partial file"), 1);
    assert_eq!(f.store.collect(10).await.expect("again"), 0);
    let used: i64 = sqlx::query_scalar("SELECT used_bytes FROM artifact_capacity WHERE root_id=$1")
        .bind(&f.store.inner.objects.root_id)
        .fetch_one(&f.store.inner.pool)
        .await
        .expect("quota");
    assert_eq!(used, 0);
    f.store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"abcdef"),
        )
        .await
        .expect("new transfer, original content");
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn active_reference_survives_deletion_retention_and_cannot_be_added_after_delete() {
    let f = fixture().await;
    let req = request(b"pinned");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let v = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"pinned"),
        )
        .await
        .expect("put");
    sqlx::query("INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,'explicit-reference-fixture')").bind(&v.artifact_id).execute(&f.store.inner.pool).await.expect("reference fixture, not Worker delivery");
    f.store
        .delete(&f.actor, &f.owner, &req.upload_id)
        .await
        .expect("logical deletion");
    assert!(
        sqlx::query(
            "INSERT INTO artifact_pins(artifact_id,reference_id) VALUES($1,'late-reference')"
        )
        .bind(&v.artifact_id)
        .execute(&f.store.inner.pool)
        .await
        .is_err()
    );
    sqlx::query("UPDATE artifacts SET deleted_at=clock_timestamp()-interval '25 hours' WHERE artifact_id=$1").bind(&v.artifact_id).execute(&f.store.inner.pool).await.expect("retention fixture");
    f.store.collect(100).await.expect("stage only");
    let used: i64 = sqlx::query_scalar("SELECT used_bytes FROM artifact_capacity WHERE root_id=$1")
        .bind(&f.store.inner.objects.root_id)
        .fetch_one(&f.store.inner.pool)
        .await
        .expect("still pinned");
    assert_eq!(used, 6);
    sqlx::query("DELETE FROM artifact_pins WHERE artifact_id=$1")
        .bind(&v.artifact_id)
        .execute(&f.store.inner.pool)
        .await
        .expect("reference release fixture");
    assert_eq!(f.store.collect(100).await.expect("purge unreferenced"), 1);
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn database_publish_failure_leaves_real_copy_invisible() {
    let f = fixture().await;
    let req = request(b"publish barrier");
    let v = f
        .store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let name = format!("artifact_fault_{}", uuid::Uuid::new_v4().simple());
    // Test-only DDL: both interpolated values are generated identifiers, never
    // request input. SQLx requires this audit for dynamic trigger definitions.
    assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    assert_eq!(
        uuid::Uuid::parse_str(&v.artifact_id)
            .expect("generated UUID")
            .to_string(),
        v.artifact_id
    );
    let ddl = format!(
        "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.artifact_id='{}' AND NEW.state='available' THEN RAISE EXCEPTION 'explicit publish fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER {name} BEFORE UPDATE ON artifacts FOR EACH ROW EXECUTE FUNCTION {name}();",
        v.artifact_id
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(&f.store.inner.pool)
        .await
        .expect("fault fixture");
    let result = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"publish barrier"),
        )
        .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {name} ON artifacts; DROP FUNCTION {name}();"
    )))
    .execute(&f.store.inner.pool)
    .await
    .expect("remove fixture");
    assert!(matches!(result, Err(ArtifactError::Database(_))));
    let key: String =
        sqlx::query_scalar("SELECT published_key FROM artifact_transfers WHERE artifact_id=$1")
            .bind(&v.artifact_id)
            .fetch_one(&f.store.inner.pool)
            .await
            .expect("copied key");
    assert_eq!(
        std::fs::read(f.root.path().join(key)).expect("real orphan bytes"),
        b"publish barrier"
    );
    assert!(matches!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &v.version_id)
            .await,
        Err(ArtifactError::NotReady)
    ));
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn dropping_request_retains_owned_io_and_original_id_recovers_outcome() {
    let f = fixture().await;
    let req = request(b"abcdef");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let (send, recv) = tokio::sync::mpsc::channel(2);
    let store = f.store.clone();
    let actor = f.actor.clone();
    let owner = f.owner.clone();
    let upload = req.upload_id.clone();
    let caller = tokio::spawn(async move {
        store
            .put(
                actor,
                owner,
                upload,
                tokio_stream::wrappers::ReceiverStream::new(recv),
            )
            .await
    });
    send.send(Ok(opendal::Buffer::from("abc")))
        .await
        .expect("data");
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.store.inner.slots.available_permits() != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned writer");
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    assert_eq!(f.store.inner.slots.available_permits(), 3);
    send.send(Ok(opendal::Buffer::from("def")))
        .await
        .expect("buffered data");
    drop(send);
    f.store.drain().await.expect("joins actual owned transfer");
    assert_eq!(f.store.inner.slots.available_permits(), 4);
    assert_eq!(
        f.store
            .status(&f.actor, &f.owner, &req.upload_id)
            .await
            .expect("query original")
            .state,
        "available"
    );
}
#[tokio::test]
async fn organization_acl_version_changes_fence_inflight_transfer_and_old_read_stamp() {
    use orange_protocol::team::*;
    let f = fixture().await;
    let creator = fixture().await;
    let team = crate::team::TeamStore::new(f.store.inner.pool.clone());
    let org = team
        .create_organization(
            &f.actor.user,
            "org",
            &CreateOrganization {
                name: "Artifact ACL fixture".into(),
                member_limit: 5,
            },
        )
        .await
        .expect("org")
        .organization_id;
    team.set_membership(
        &f.actor.user,
        &org,
        "member",
        &ChangeMembership {
            user_id: creator.actor.user.clone(),
            role: Some(TeamRole::Member),
            expected_policy_version: 1,
        },
    )
    .await
    .expect("member");
    let req = request(b"abcdef");
    let v = f
        .store
        .begin_upload(&creator.actor, &org, &req)
        .await
        .expect("member private artifact");
    assert!(
        matches!(
            f.store.status(&f.actor, &org, &req.upload_id).await,
            Err(ArtifactError::Forbidden)
        ),
        "owner role is not a private-content ACL"
    );
    let grant = |access, version| ChangeResourceAcl {
        user_id: f.actor.user.clone(),
        access,
        expected_acl_version: version,
    };
    team.set_acl(
        &creator.actor.user,
        &org,
        &v.artifact_id,
        "grant",
        &grant(Some(ResourceAccess::Write), 1),
    )
    .await
    .expect("explicit grant");
    let (send, recv) = tokio::sync::mpsc::channel(2);
    let store = f.store.clone();
    let actor = f.actor.clone();
    let own = org.clone();
    let upload = req.upload_id.clone();
    let task = tokio::spawn(async move {
        store
            .put(
                actor,
                own,
                upload,
                tokio_stream::wrappers::ReceiverStream::new(recv),
            )
            .await
    });
    send.send(Ok(opendal::Buffer::from("abc")))
        .await
        .expect("part");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM artifact_transfers WHERE artifact_id=$1")
                    .bind(&v.artifact_id)
                    .fetch_one(&f.store.inner.pool)
                    .await
                    .expect("writing");
            if n == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prepared");
    team.set_acl(
        &creator.actor.user,
        &org,
        &v.artifact_id,
        "revoke",
        &grant(None, 2),
    )
    .await
    .expect("revoke");
    team.set_acl(
        &creator.actor.user,
        &org,
        &v.artifact_id,
        "regrant",
        &grant(Some(ResourceAccess::Write), 3),
    )
    .await
    .expect("explicit new grant");
    send.send(Ok(opendal::Buffer::from("def")))
        .await
        .expect("rest");
    drop(send);
    assert!(matches!(
        task.await.expect("task"),
        Err(ArtifactError::Forbidden)
    ));
    f.store
        .put(
            f.actor.clone(),
            org.clone(),
            req.upload_id.clone(),
            input(b"abcdef"),
        )
        .await
        .expect("fresh request under new grant");
    let (view, _, stamp) = f
        .store
        .read(&f.actor, &org, &req.upload_id, &v.version_id)
        .await
        .expect("read");
    team.set_acl(
        &creator.actor.user,
        &org,
        &v.artifact_id,
        "revoke-again",
        &grant(None, 4),
    )
    .await
    .expect("revoke");
    team.set_acl(
        &creator.actor.user,
        &org,
        &v.artifact_id,
        "regrant-again",
        &grant(Some(ResourceAccess::Write), 5),
    )
    .await
    .expect("new grant");
    assert!(f.store.check_read(&f.actor, &view, &stamp).await.is_err());
    f.store
        .read(&f.actor, &org, &req.upload_id, &v.version_id)
        .await
        .expect("fresh authorized read");
    f.store.drain().await.expect("drain");
    creator.store.drain().await.expect("drain");
}

#[tokio::test]
async fn real_http_enforces_cookie_csrf_streaming_integrity_and_revocation() {
    use crate::auth::{
        hash_secret, random_secret,
        users::{USER_COOKIE, UserIdentity, UserOidcConfig},
    };
    let f = fixture().await;
    let token = random_secret();
    let csrf = random_secret();
    sqlx::query("UPDATE user_sessions SET token_hash=$2,csrf_token=$3 WHERE session_id=$1")
        .bind(&f.actor.session)
        .bind(hash_secret(&token))
        .bind(&csrf)
        .execute(&f.store.inner.pool)
        .await
        .expect("HTTP identity fixture");
    let config: UserOidcConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("ORANGE_TEST_USER_CONFIG_FILE").expect("provider config"))
            .expect("config"),
    )
    .expect("config parse");
    let origin = config.user_origin.clone();
    assert_eq!(config.environment, "dev");
    let identity = UserIdentity::discover(f.store.inner.pool.clone(), config)
        .await
        .expect("real local discovery");
    let app = crate::auth::routes::router(identity.clone())
        .merge(super::http::router(identity, f.store.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("actual listener");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client");
    let cookie = format!("{USER_COOKIE}={token}");
    let bytes = vec![b'x'; 128 * 1024];
    let req = request(&bytes);
    let collection = format!("{base}/api/v1/owners/{}/uploads", f.owner);
    assert_eq!(
        client
            .post(&collection)
            .json(&req)
            .send()
            .await
            .expect("unauthenticated")
            .status(),
        401
    );
    assert_eq!(
        client
            .post(&collection)
            .header("cookie", &cookie)
            .header("origin", &origin)
            .json(&req)
            .send()
            .await
            .expect("CSRF missing")
            .status(),
        403
    );
    assert_eq!(
        client
            .post(&collection)
            .header("cookie", &cookie)
            .header("origin", &origin)
            .header("x-csrf-token", &csrf)
            .header("idempotency-key", "different-upload-id")
            .json(&req)
            .send()
            .await
            .expect("mismatched request key")
            .status(),
        409
    );
    let response = client
        .post(&collection)
        .header("cookie", &cookie)
        .header("origin", &origin)
        .header("x-csrf-token", &csrf)
        .header("idempotency-key", &req.upload_id)
        .json(&req)
        .send()
        .await
        .expect("create");
    assert_eq!(response.status(), 200);
    let view: ArtifactView = response.json().await.expect("metadata");
    let content = format!("{collection}/{}/content", req.upload_id);
    let response = client
        .put(&content)
        .header("cookie", &cookie)
        .header("origin", &origin)
        .header("x-csrf-token", &csrf)
        .header("idempotency-key", &req.upload_id)
        .body(bytes.clone())
        .send()
        .await
        .expect("upload beyond JSON64KiB limit");
    assert_eq!(response.status(), 200);
    let ready: ArtifactView = response.json().await.expect("published");
    assert_eq!(ready.version_id, view.version_id);
    assert_eq!(ready.state, "available");
    let read = format!("{content}?version_id={}", view.version_id);
    let response = client
        .get(&read)
        .header("cookie", &cookie)
        .send()
        .await
        .expect("download");
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["content-disposition"], "attachment");
    assert_eq!(
        response.bytes().await.expect("bounded stream").as_ref(),
        bytes.as_slice()
    );
    sqlx::query("UPDATE user_sessions SET revoked=TRUE WHERE session_id=$1")
        .bind(&f.actor.session)
        .execute(&f.store.inner.pool)
        .await
        .expect("revoke");
    assert_eq!(
        client
            .get(&read)
            .header("cookie", &cookie)
            .send()
            .await
            .expect("after revoke")
            .status(),
        401
    );
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("shutdown")
        .expect("join")
        .expect("serve");
    f.store.drain().await.expect("drain");
}

#[tokio::test]
async fn expired_empty_upload_releases_active_quota_without_inventing_user_deletion() {
    let f = fixture().await;
    let req = request(b"not uploaded");
    let v = f
        .store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    sqlx::query(
        "UPDATE artifacts SET created_at=clock_timestamp()-interval '2 hours' WHERE artifact_id=$1",
    )
    .bind(&v.artifact_id)
    .execute(&f.store.inner.pool)
    .await
    .expect("clock fixture");
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(b"not uploaded")
            )
            .await,
        Err(ArtifactError::Expired)
    ));
    f.store.collect(100).await.expect("expire staging metadata");
    assert_eq!(
        f.store
            .status(&f.actor, &f.owner, &req.upload_id)
            .await
            .expect("original expired ID")
            .state,
        "deleted"
    );
    let (records, bytes): (i64, i64) =
        sqlx::query_as("SELECT records,used_bytes FROM artifact_capacity WHERE root_id=$1")
            .bind(&f.store.inner.objects.root_id)
            .fetch_one(&f.store.inner.pool)
            .await
            .expect("quota");
    assert_eq!((records, bytes), (0, 0));
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM artifact_expirations WHERE artifact_id=$1),(SELECT count(*) FROM artifact_tombstones WHERE artifact_id=$1)").bind(&v.artifact_id).fetch_one(&f.store.inner.pool).await.expect("facts");
    assert_eq!(counts, (1, 0));
    f.store
        .delete(&f.actor, &f.owner, &req.upload_id)
        .await
        .expect("idempotent deleted result");
    f.store.collect(100).await.expect("no duplicate expiry");
    f.store.drain().await.expect("drain");
}
#[tokio::test]
async fn object_and_media_limits_empty_objects_and_actual_100mib_transfer() {
    let f = fixture().await;
    let mut request = CreateUpload {
        upload_id: id(),
        sha256: "a".repeat(64),
        size_bytes: (orange_protocol::artifacts::OBJECT_LIMIT + 1).to_string(),
        media_type: "application/octet-stream".into(),
    };
    assert!(matches!(
        f.store.begin_upload(&f.actor, &f.owner, &request).await,
        Err(ArtifactError::Invalid)
    ));
    request.media_type = "image/png".into();
    request.size_bytes = (orange_protocol::artifacts::MEDIA_LIMIT + 1).to_string();
    assert!(matches!(
        f.store.begin_upload(&f.actor, &f.owner, &request).await,
        Err(ArtifactError::Invalid)
    ));
    let empty = super::tests::request(b"");
    f.store
        .begin_upload(&f.actor, &f.owner, &empty)
        .await
        .expect("empty begin");
    let v = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            empty.upload_id.clone(),
            input(b""),
        )
        .await
        .expect("empty immutable file");
    assert!(
        f.store
            .read(&f.actor, &f.owner, &empty.upload_id, &v.version_id)
            .await
            .expect("empty read")
            .1
            .is_empty()
    );
    let big_root = tempfile::tempdir().expect("big root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(big_root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private");
    }
    let big = ArtifactStore::open_local(
        f.store.inner.pool.clone(),
        big_root.path(),
        3 * orange_protocol::artifacts::OBJECT_LIMIT,
        10,
    )
    .await
    .expect("bounded real disk quota");
    let chunk = vec![0x5a; 64 * 1024];
    let mut hash = Sha256::new();
    for _ in 0..1600 {
        hash.update(&chunk);
    }
    let req = CreateUpload {
        upload_id: id(),
        sha256: hex::encode(hash.finalize()),
        size_bytes: orange_protocol::artifacts::OBJECT_LIMIT.to_string(),
        media_type: "application/octet-stream".into(),
    };
    big.begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("max begin");
    let stream =
        tokio_stream::iter(0..1600).map(|_| Ok(opendal::Buffer::from(vec![0x5a; 64 * 1024])));
    let ready = big
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            stream,
        )
        .await
        .expect("actual100MiB upload/copy");
    let (_, data, _) = big
        .read(&f.actor, &f.owner, &req.upload_id, &ready.version_id)
        .await
        .expect("actual100MiB read");
    assert_eq!(data.len() as u64, orange_protocol::artifacts::OBJECT_LIMIT);
    let mut actual = Sha256::new();
    for part in data {
        actual.update(&part);
    }
    assert_eq!(hex::encode(actual.finalize()), req.sha256);
    big.drain().await.expect("drain big");
    f.store.drain().await.expect("drain");
}

#[tokio::test]
async fn concurrent_original_upload_has_one_writer_and_capacity_refuses_before_io() {
    let f = fixture().await;
    let req = request(b"abcdef");
    let v = f
        .store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let (send, recv) = tokio::sync::mpsc::channel(2);
    let store = f.store.clone();
    let actor = f.actor.clone();
    let owner = f.owner.clone();
    let upload = req.upload_id.clone();
    let first = tokio::spawn(async move {
        store
            .put(
                actor,
                owner,
                upload,
                tokio_stream::wrappers::ReceiverStream::new(recv),
            )
            .await
    });
    send.send(Ok(opendal::Buffer::from("abc")))
        .await
        .expect("part");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM artifact_transfers WHERE artifact_id=$1")
                    .bind(&v.artifact_id)
                    .fetch_one(&f.store.inner.pool)
                    .await
                    .expect("started");
            if n == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer");
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                req.upload_id.clone(),
                input(b"abcdef")
            )
            .await,
        Err(ArtifactError::Busy)
    ));
    send.send(Ok(opendal::Buffer::from("def")))
        .await
        .expect("rest");
    drop(send);
    first.await.expect("task").expect("first wins");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact_transfers WHERE artifact_id=$1")
            .bind(&v.artifact_id)
            .fetch_one(&f.store.inner.pool)
            .await
            .expect("count");
    assert_eq!(count, 1);
    let too_big = vec![1; 600_000];
    let request = request(&too_big);
    let blocked = f
        .store
        .begin_upload(&f.actor, &f.owner, &request)
        .await
        .expect("declared intent");
    assert!(matches!(
        f.store
            .put(
                f.actor.clone(),
                f.owner.clone(),
                request.upload_id.clone(),
                input(&too_big)
            )
            .await,
        Err(ArtifactError::Capacity)
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact_transfers WHERE artifact_id=$1")
            .bind(&blocked.artifact_id)
            .fetch_one(&f.store.inner.pool)
            .await
            .expect("no write");
    assert_eq!(count, 0);
    f.store.drain().await.expect("drain");
}
#[cfg(unix)]
#[tokio::test]
async fn a_replaced_link_cannot_expose_external_file_bytes() {
    use std::os::unix::fs::symlink;
    let f = fixture().await;
    let req = request(b"safe");
    f.store
        .begin_upload(&f.actor, &f.owner, &req)
        .await
        .expect("begin");
    let v = f
        .store
        .put(
            f.actor.clone(),
            f.owner.clone(),
            req.upload_id.clone(),
            input(b"safe"),
        )
        .await
        .expect("publish");
    let key: String = sqlx::query_scalar(
        "SELECT published_key FROM artifact_transfers WHERE artifact_id=$1 AND state='attached'",
    )
    .bind(&v.artifact_id)
    .fetch_one(&f.store.inner.pool)
    .await
    .expect("key");
    let outside = tempfile::tempdir().expect("owned external fixture");
    let secret = outside.path().join("secret");
    std::fs::write(&secret, b"safe").expect("known hash alone is not path authorization");
    let path = f.root.path().join(&key);
    std::fs::remove_file(&path).expect("replace fixture");
    symlink(&secret, &path).expect("link fixture");
    assert!(matches!(
        f.store
            .read(&f.actor, &f.owner, &req.upload_id, &v.version_id)
            .await,
        Err(ArtifactError::StoragePolicy)
    ));
    assert_eq!(std::fs::read(&secret).expect("external unchanged"), b"safe");
    f.store.drain().await.expect("drain");
}
