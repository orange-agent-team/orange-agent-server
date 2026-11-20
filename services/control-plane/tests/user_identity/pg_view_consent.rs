use super::*;
use orange_control_plane::pg_view_sources::Sources;
use sha2::{Digest, Sha256};
use std::io::Write;

use super::connector_run::keyed;

#[tokio::test]
async fn scoped_source_reads_require_live_run_and_grant_at_delivery() {
    let mut f = fixture().await;
    let other = fixture().await;
    let source =
        sqlx::PgPool::connect(&std::env::var("ORANGE_VIEW_FIXTURE_ADMIN").expect("fixture"))
            .await
            .expect("source pool");
    let mut config: Value =
        serde_json::from_str(&std::env::var("ORANGE_VIEW_FIXTURE_CONFIG").expect("fixture config"))
            .expect("source config");
    let def: String = sqlx::query_scalar("SELECT pg_get_viewdef('source.sales'::regclass,false)")
        .fetch_one(&source)
        .await
        .expect("definition");
    config["registration"]["views"]["sales"]["definition_sha256"] =
        json!(hex::encode(Sha256::digest(def)));
    config["registration"]["views"]["sales"]["relation_oid"] = json!(
        sqlx::query_scalar::<_, i64>("SELECT 'source.sales'::regclass::oid::bigint")
            .fetch_one(&source)
            .await
            .expect("view OID")
    );
    config["owner_id"] = json!(f.user);
    let mut file = tempfile::NamedTempFile::new().expect("private source file");
    file.write_all(&serde_json::to_vec(&[&config]).expect("fixture bytes"))
        .expect("write file");
    f.identity.pg_sources = Sources::load_development_file(file.path()).expect("registered source");
    f.router = auth::routes::router(f.identity.clone());
    let (run, _root) = super::connector_run::active_run(&f).await;
    if let Ok(driver) = std::env::var("ORANGE_CN05_BROWSER") {
        let receipt = super::connector_run::browser(&f, &run, driver).await;
        assert_eq!(
            receipt,
            json!({"approved":true,"listed":true,"described":true,"queried":true,"revoked_after_reload":true})
        );
    }
    let base = format!("/api/v1/agent/runs/{run}/connectors/postgres");
    assert_eq!(
        request(&other, "GET", &base, None, headers(&other)).await.0,
        StatusCode::NOT_FOUND
    );
    let (code, list) = request(&f, "GET", &base, None, headers(&f)).await;
    assert_eq!(code, StatusCode::OK);
    let terms = json!({"source_id":"sales-db","source_digest":list["sources"][0]["source_digest"],"view_ids":["sales"],"duration_seconds":60});
    let (code, g) = request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve")).await;
    assert_eq!(code, StatusCode::OK, "{g}");
    assert_eq!(
        request(&f, "POST", &base, Some(terms.clone()), keyed(&f, "approve"))
            .await
            .1,
        g
    );
    let grant = format!("{base}/{}", g["grant_id"].as_str().expect("grant id"));
    let actions = format!("{grant}/actions");
    let query = json!({"action":"query_view","view_id":"sales","columns":["seq"],"limit":1});
    let (code, rows) = request(&f, "POST", &actions, Some(query.clone()), keyed(&f, "read")).await;
    assert_eq!(code, StatusCode::OK, "{rows}");
    assert_eq!(rows["data"]["rows"], json!([{"seq":"10"}]));
    assert_eq!(
        request(&f, "POST", &actions, Some(query.clone()), keyed(&f, "read"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut lock = source.begin().await.expect("source fence");
    sqlx::query("LOCK source.sales IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("fence");
    let reading = request(
        &f,
        "POST",
        &actions,
        Some(query.clone()),
        keyed(&f, "inflight"),
    );
    let revoking = async {
        tokio::time::timeout(std::time::Duration::from_secs(3),async{
            loop{
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE usename=$1 AND wait_event_type='Lock')").bind(config["registration"]["read_role"].as_str().expect("role")).fetch_one(&source).await.expect("check wait");
                if waiting{break;}tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("connector reached source");
        assert_eq!(
            request(&f, "DELETE", &grant, None, headers(&f)).await.0,
            StatusCode::NO_CONTENT
        );
        lock.commit().await.expect("release");
    };
    let ((code, denial), ()) = tokio::join!(reading, revoking);
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(denial.get("data").is_none());
    let (code, new_grant) =
        request(&f, "POST", &base, Some(terms), keyed(&f, "approve-again")).await;
    assert_eq!(code, StatusCode::OK);
    assert_ne!(new_grant, g);
    assert_eq!(
        request(&f, "POST", &actions, Some(query), keyed(&f, "old"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let new_id = new_grant["grant_id"].as_str().expect("new grant");
    let current = format!("{base}/{new_id}/actions");
    // Set up a real occupied slot window and then the exact final quota boundary.
    sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("end old window");
    sqlx::query("INSERT INTO pg_view_calls(run_id,grant_id,request_key,until_at) SELECT $1,$2,'busy-'||i,clock_timestamp()+interval '30 seconds' FROM generate_series(1,4) i").bind(&run).bind(new_id).execute(&f.pool).await.expect("four active slots");
    let list = json!({"action":"list_views"});
    assert_eq!(
        request(&f, "POST", &current, Some(list.clone()), keyed(&f, "fifth"))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE pg_view_calls SET finished_at=clock_timestamp() WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("release slots");
    sqlx::query("INSERT INTO pg_view_calls(run_id,grant_id,request_key,until_at,finished_at) SELECT $1,$2,'past-'||i,clock_timestamp(),clock_timestamp() FROM generate_series(1,99-(SELECT count(*) FROM pg_view_calls WHERE run_id=$1)) i").bind(&run).bind(new_id).execute(&f.pool).await.expect("99 calls");
    assert_eq!(
        request(
            &f,
            "POST",
            &current,
            Some(list.clone()),
            keyed(&f, "hundredth")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&f, "POST", &current, Some(list), keyed(&f, "over"))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE cloud_runs SET status='cancelled',version=version+1 WHERE run_id=$1")
        .bind(&run)
        .execute(&f.pool)
        .await
        .expect("terminal Run fixture");
    let (code, inventory) = request(&f, "GET", &base, None, headers(&f)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(inventory["run_readable"], false);
    assert_eq!(
        request(
            &f,
            "POST",
            &current,
            Some(json!({"action":"list_views"})),
            keyed(&f, "terminal")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // Revoke the source's SELECT privilege. Restoring it does not reopen an old platform grant.
    let mut fresh = fixture().await;
    let (run2, _root2) = super::connector_run::active_run(&fresh).await;
    config["owner_id"] = json!(fresh.user);
    fn configure(f: &mut Fixture, c: &Value) {
        let mut file = tempfile::NamedTempFile::new().expect("private configuration");
        file.write_all(&serde_json::to_vec(&[c]).expect("json"))
            .expect("write");
        f.identity.pg_sources = Sources::load_development_file(file.path()).expect("load");
        f.router = auth::routes::router(f.identity.clone());
    }
    configure(&mut fresh, &config);
    let base2 = format!("/api/v1/agent/runs/{run2}/connectors/postgres");
    let inv = request(&fresh, "GET", &base2, None, headers(&fresh))
        .await
        .1;
    let mut terms = json!({"source_id":"sales-db","source_digest":inv["sources"][0]["source_digest"],"view_ids":["sales"],"duration_seconds":120});
    let grant2 = request(
        &fresh,
        "POST",
        &base2,
        Some(terms.clone()),
        keyed(&fresh, "source-grant"),
    )
    .await
    .1;
    let acts = format!(
        "{base2}/{}/actions",
        grant2["grant_id"].as_str().expect("grant")
    );
    let role = config["registration"]["read_role"].as_str().expect("role");
    assert!(
        role.len() == 37
            && role.starts_with("cn05_")
            && role[5..].bytes().all(|b| b.is_ascii_hexdigit())
    );
    sqlx::QueryBuilder::new("REVOKE SELECT ON source.sales FROM ")
        .push(role)
        .build()
        .execute(&source)
        .await
        .expect("fixture role revoke");
    let action = json!({"action":"list_views"});
    assert_eq!(
        request(
            &fresh,
            "POST",
            &acts,
            Some(action.clone()),
            keyed(&fresh, "refused")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::QueryBuilder::new("GRANT SELECT ON source.sales TO ")
        .push(role)
        .build()
        .execute(&source)
        .await
        .expect("fixture role restore");
    configure(&mut fresh, &config);
    assert_eq!(
        request(&fresh, "GET", &base2, None, headers(&fresh))
            .await
            .1["sources"],
        json!([])
    );
    assert_eq!(
        request(
            &fresh,
            "POST",
            &acts,
            Some(action.clone()),
            keyed(&fresh, "still-disabled")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_view_calls WHERE run_id=$1")
        .bind(&run2)
        .fetch_one(&fresh.pool)
        .await
        .expect("dispatch count");
    assert_eq!(count, 1);
    config["credential_revision"] = json!("restored-v2");
    configure(&mut fresh, &config);
    let inv = request(&fresh, "GET", &base2, None, headers(&fresh))
        .await
        .1;
    assert_ne!(terms["source_digest"], inv["sources"][0]["source_digest"]);
    terms["source_digest"] = inv["sources"][0]["source_digest"].clone();
    let g3 = request(
        &fresh,
        "POST",
        &base2,
        Some(terms),
        keyed(&fresh, "new-source"),
    )
    .await
    .1;
    let acts = format!(
        "{base2}/{}/actions",
        g3["grant_id"].as_str().expect("fresh grant")
    );
    assert_eq!(
        request(
            &fresh,
            "POST",
            &acts,
            Some(action),
            keyed(&fresh, "recovered")
        )
        .await
        .0,
        StatusCode::OK
    );
    source.close().await;
}
