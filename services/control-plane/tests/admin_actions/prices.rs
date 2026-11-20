//! Real scoped Admin HTTP/PG; the identity, execution and meter admissions are fixtures.
use super::*;
use orange_control_plane::admin::activate_due_text_prices;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const DRAFTS: &str = "/admin/v1/price-drafts";
const ACTIONS: &str = "/admin/v1/actions";

async fn admit(f: &Fixture) -> Value {
    let owner: String = sqlx::query_scalar("SELECT owner_id FROM nodes WHERE node_id=$1")
        .bind(&f.node)
        .fetch_one(&f.pool)
        .await
        .expect("owner");
    for op in [&f.maker, &f.checker] {
        sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'finance_operator','dev','tenant',$2,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)")
            .bind(&op.id).bind(&owner).execute(&f.pool).await.expect("finance tenant fixture");
    }
    crate::price_fixture::seed(&f.pool, &owner, &f.node).await
}
async fn draft(f: &Fixture, terms: &Value, key: &str) -> Value {
    let (status, row, _) = http(f, &f.maker, "POST", DRAFTS, key, "", terms.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["terms"], *terms);
    row
}
fn price_id(row: &Value) -> &str {
    row["price_version_id"].as_str().expect("version")
}
fn publish(row: &Value, at: &str) -> Value {
    json!({"scope":row["terms"]["scope"],"target_version":row["target_version"],"ticket_id":"price-review","reason":"Reviewed development credit schedule","action_type":"pricing.publish","payload":{"price_version_id":row["price_version_id"],"review_report_id":row["review_report_id"],"effective_at":at}})
}
async fn active_count(f: &Fixture, row: &Value) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM text_prices WHERE price_version=$1")
        .bind(price_id(row))
        .fetch_one(&f.pool)
        .await
        .expect("quotable catalog")
}
async fn release(f: &Fixture, row: &Value, at: &str) -> Value {
    let (status, action, tag) = http(
        f,
        &f.maker,
        "POST",
        ACTIONS,
        price_id(row),
        "",
        publish(row, at),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{action}");
    assert_eq!(action["approval_policy"], "maker_checker");
    assert_eq!(
        http(
            f,
            &f.maker,
            "POST",
            &path(&action, "approvals"),
            "self",
            &tag,
            decision(&action, "approve")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, approval, tag) = http(
        f,
        &f.checker,
        "POST",
        &path(&action, "approvals"),
        price_id(row),
        &tag,
        decision(&action, "approve"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approval}");
    let id = price_id(row);
    let key = format!("execute-{id}");
    let endpoint = path(&action, "execute");
    let (status, done, _) =
        http(f, &f.maker, "POST", &endpoint, &key, &tag, preview(&action)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{done}");
    assert_eq!(done["business_ref"], row["price_version_id"]);
    let (again, replayed, _) =
        http(f, &f.maker, "POST", &endpoint, &key, &tag, preview(&action)).await;
    assert_eq!(again, status);
    assert_eq!(done, replayed);
    done
}
fn read_path(row: &Value) -> String {
    format!(
        "{DRAFTS}/{}?environment=dev&scope_kind=tenant&scope_id={}",
        price_id(row),
        row["terms"]["scope"]["scope_id"].as_str().expect("tenant")
    )
}

#[tokio::test]
async fn scoped_drafts_validate_terms_deduplicate_and_require_independent_publication() {
    let f = fixture().await;
    let terms = admit(&f).await;
    for (key, value) in [
        ("fee_bps", json!(10001)),
        ("output_limit", json!(0)),
        ("input_price", json!("0001")),
        ("input_price", json!("9223372036854775807")),
        ("unknown", json!("danger")),
    ] {
        let mut invalid = terms.clone();
        invalid[key] = value;
        assert_eq!(
            http(&f, &f.maker, "POST", DRAFTS, "bad", "", invalid)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut mismatch = terms.clone();
    mismatch["meter_profile_digest"] = json!(hash_secret("missing"));
    assert_eq!(
        http(&f, &f.maker, "POST", DRAFTS, "bad", "", mismatch)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        http(&f, &f.stranger, "POST", DRAFTS, "deny", "", terms.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let row = draft(&f, &terms, "draft-1").await;
    assert_eq!(row["state"], "draft");
    assert_eq!(row, draft(&f, &terms, "draft-1").await);
    let mut changed = terms.clone();
    changed["fee_bps"] = json!(10);
    assert_eq!(
        http(&f, &f.maker, "POST", DRAFTS, "draft-1", "", changed.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let old = draft(&f, &changed, "draft-2").await;
    let list = format!(
        "{DRAFTS}?environment=dev&scope_kind=tenant&scope_id={}&limit=1",
        terms["scope"]["scope_id"].as_str().expect("tenant")
    );
    let (status, first, _) = http(&f, &f.maker, "GET", &list, "", "", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"][0], old);
    let next = format!(
        "{list}&cursor={}",
        first["next_cursor"].as_str().expect("cursor")
    );
    draft(&f, &terms, "later").await;
    assert_eq!(
        http(&f, &f.checker, "GET", &next, "", "", Value::Null)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        http(&f, &f.maker, "GET", &next, "", "", Value::Null)
            .await
            .1,
        json!({"data":[row]})
    );
    assert_eq!(
        http(
            &f,
            &f.stranger,
            "GET",
            &read_path(&row),
            "",
            "",
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let at = row["created_at"].as_str().expect("creation time");
    let mut stale = publish(&row, at);
    stale["target_version"] = json!(hash_secret("stale"));
    assert_eq!(
        http(&f, &f.maker, "POST", ACTIONS, "stale", "", stale)
            .await
            .0,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        http(
            &f,
            &f.checker,
            "POST",
            ACTIONS,
            "another-maker",
            "",
            publish(&row, at)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(active_count(&f, &row).await, 0);
    release(&f, &row, at).await;
    assert_eq!(active_count(&f, &row).await, 1);
    assert_eq!(active_count(&f, &old).await, 0);
    assert_eq!(
        http(&f, &f.maker, "GET", &read_path(&row), "", "", Value::Null)
            .await
            .1["state"],
        "active"
    );
    let fixed:(i64,i64,i64,i32)=sqlx::query_as("SELECT input_price,output_price,output_limit,fee_bps FROM text_prices WHERE price_version=$1").bind(price_id(&row)).fetch_one(&f.pool).await.expect("published rates");
    assert_eq!(fixed, (2000, 6000, 128, 120));
    assert!(
        sqlx::query("UPDATE text_prices SET input_price=1 WHERE price_version=$1")
            .bind(price_id(&row))
            .execute(&f.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn scheduled_release_refuses_disabled_meter() {
    scheduled("meter").await;
}
#[tokio::test]
async fn scheduled_release_refuses_revoked_independent_checker() {
    scheduled("checker").await;
}
#[tokio::test]
async fn scheduled_release_recovers_once_across_concurrent_schedulers() {
    scheduled("active").await;
}
async fn scheduled(mode: &str) {
    let f = fixture().await;
    let terms = admit(&f).await;
    let row = draft(&f, &terms, "scheduled").await;
    let now: i64 =
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
            .fetch_one(&f.pool)
            .await
            .expect("database clock");
    let at = OffsetDateTime::from_unix_timestamp(now + 4)
        .expect("scheduled instant")
        .format(&Rfc3339)
        .expect("UTC timestamp");
    release(&f, &row, &at).await;
    assert_eq!(active_count(&f, &row).await, 0);
    assert_eq!(
        http(&f, &f.maker, "GET", &read_path(&row), "", "", Value::Null)
            .await
            .1["state"],
        "scheduled"
    );
    if mode == "checker" {
        sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND role='finance_operator'")
            .bind(&f.checker.id).execute(&f.pool).await.expect("revoke checker before effectiveness");
    }
    if mode == "meter" {
        sqlx::query("UPDATE trusted_meter_profiles SET enabled=false WHERE profile_digest=$1")
            .bind(
                terms["meter_profile_digest"]
                    .as_str()
                    .expect("meter reference"),
            )
            .execute(&f.pool)
            .await
            .expect("withdraw fixture meter");
    }
    let recovered =
        PgPool::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("isolated database URL"))
            .await
            .expect("restart connection");
    sqlx::query("SELECT pg_sleep(GREATEST(0, $1::bigint - EXTRACT(EPOCH FROM clock_timestamp())))")
        .bind(now + 4)
        .execute(&recovered)
        .await
        .expect("wait for database due time");
    let (a, b) = tokio::join!(
        activate_due_text_prices(&f.pool),
        activate_due_text_prices(&recovered)
    );
    a.expect("first scheduler");
    b.expect("second scheduler");
    assert_eq!(
        http(&f, &f.maker, "GET", &read_path(&row), "", "", Value::Null)
            .await
            .1["state"],
        if mode == "active" {
            "active"
        } else {
            "blocked"
        }
    );
    sqlx::query("UPDATE trusted_meter_profiles SET enabled=true WHERE profile_digest=$1")
        .bind(
            terms["meter_profile_digest"]
                .as_str()
                .expect("meter reference"),
        )
        .execute(&f.pool)
        .await
        .expect("restore fixture meter");
    activate_due_text_prices(&recovered)
        .await
        .expect("retry scheduler");
    assert_eq!(active_count(&f, &row).await, i64::from(mode == "active"));
    let event = match mode {
        "active" => "pricing.publication_activated",
        "checker" => "pricing.publication_authority_lost",
        _ => "pricing.publication_admission_lost",
    };
    let final_events:Vec<String>=sqlx::query_scalar("SELECT a.event_type FROM admin_audit a JOIN admin_text_price_publications p ON p.action_id=a.entity_id WHERE p.price_version=$1 AND a.principal_id='system.price-scheduler'")
        .bind(price_id(&row)).fetch_all(&f.pool).await.expect("terminal publication audit");
    assert_eq!(final_events, vec![event.to_owned()]);
    recovered.close().await;
}

#[tokio::test]
async fn transcription_rates_require_independent_approval_and_match_the_reviewed_observation() {
    const AUDIO: &str = "/admin/v1/transcription-price-drafts";
    let f = fixture().await;
    let text_terms = admit(&f).await;
    let scope = &text_terms["scope"];
    let owner = scope["scope_id"].as_str().expect("tenant");
    let terms = crate::price_fixture::seed_audio(&f.pool, owner, &f.node).await;
    let observation = terms["observation_id"].as_str().expect("observation");
    assert_eq!(
        http(&f, &f.maker, "POST", AUDIO, "draft", "", terms.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("INSERT INTO transcription_process_identities VALUES($1,$2)")
        .bind(observation)
        .bind("a".repeat(64))
        .execute(&f.pool)
        .await
        .expect("prior reviewed identity fixture");
    let (status, row, _) = http(&f, &f.maker, "POST", AUDIO, "draft", "", terms.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["node_id"], f.node);
    assert_eq!(row["terms"], terms);
    assert_eq!(
        http(&f, &f.maker, "POST", AUDIO, "draft", "", terms.clone())
            .await
            .1,
        row
    );
    let list = format!("{AUDIO}?environment=dev&scope_kind=tenant&scope_id={owner}");
    assert_eq!(
        http(&f, &f.maker, "GET", &list, "", "", Value::Null)
            .await
            .1,
        json!({"data":[row]})
    );
    assert_eq!(
        http(&f, &f.stranger, "GET", &list, "", "", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let detail = format!(
        "{AUDIO}/{}?environment=dev&scope_kind=tenant&scope_id={owner}",
        price_id(&row)
    );
    assert_eq!(
        http(&f, &f.maker, "GET", &detail, "", "", Value::Null)
            .await
            .1,
        row
    );
    let mut tampered = terms.clone();
    tampered["price_per_minute"] = json!("0");
    assert_eq!(
        http(&f, &f.maker, "POST", AUDIO, "invalid", "", tampered)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM transcription_prices WHERE node_id=$1")
            .bind(&f.node)
            .fetch_one(&f.pool)
            .await
            .expect("not priced yet"),
        0
    );
    release(&f, &row, row["created_at"].as_str().expect("time")).await;
    let actual:(i64,i32,bool,String)=sqlx::query_as("SELECT price_per_minute,fee_bps,enabled,review_digest FROM transcription_prices WHERE price_version=$1 AND admission='development_only'").bind(price_id(&row)).fetch_one(&f.pool).await.expect("approved price materialized");
    assert_eq!(
        actual,
        (
            12000,
            2000,
            true,
            row["review_report_id"].as_str().expect("review").into()
        )
    );
    assert_eq!(
        http(&f, &f.maker, "GET", &detail, "", "", Value::Null)
            .await
            .1["state"],
        "active"
    );
}

#[tokio::test]
async fn transcription_publication_rechecks_checker_at_effective_time_and_never_revives() {
    let f = fixture().await;
    let text = admit(&f).await;
    let owner = text["scope"]["scope_id"].as_str().expect("tenant");
    let terms = crate::price_fixture::seed_audio(&f.pool, owner, &f.node).await;
    sqlx::query("INSERT INTO transcription_process_identities VALUES($1,$2)")
        .bind(terms["observation_id"].as_str().expect("observation"))
        .bind("e".repeat(64))
        .execute(&f.pool)
        .await
        .expect("reviewed process");
    let (status, row, _) = http(
        &f,
        &f.maker,
        "POST",
        "/admin/v1/transcription-price-drafts",
        "scheduled",
        "",
        terms,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let at: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint+4")
            .fetch_one(&f.pool)
            .await
            .expect("due time");
    release(
        &f,
        &row,
        &OffsetDateTime::from_unix_timestamp(at)
            .expect("instant")
            .format(&Rfc3339)
            .expect("UTC"),
    )
    .await;
    sqlx::query(
        "UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND role='finance_operator'",
    )
    .bind(&f.checker.id)
    .execute(&f.pool)
    .await
    .expect("revoke checker before publication");
    sqlx::query("SELECT pg_sleep(GREATEST(0,$1::bigint-extract(epoch FROM clock_timestamp())))")
        .bind(at)
        .execute(&f.pool)
        .await
        .expect("advance to scheduled time");
    let (first, second) = tokio::join!(
        activate_due_text_prices(&f.pool),
        activate_due_text_prices(&f.pool)
    );
    first.expect("scheduler");
    second.expect("concurrent scheduler");
    let missed:(String,i64)=sqlx::query_as("SELECT p.state,(SELECT count(*) FROM transcription_prices WHERE price_version=p.price_version) FROM admin_transcription_price_publications p WHERE price_version=$1")
       .bind(price_id(&row)).fetch_one(&f.pool).await.expect("blocked record");
    assert_eq!(missed, ("blocked".into(), 0));
    sqlx::query(
        "UPDATE admin_grants SET revoked=false WHERE principal_id=$1 AND role='finance_operator'",
    )
    .bind(&f.checker.id)
    .execute(&f.pool)
    .await
    .expect("restore test grant");
    activate_due_text_prices(&f.pool).await.expect("retry");
    let events:i64=sqlx::query_scalar("SELECT count(*) FROM admin_audit WHERE event_type='pricing.publication_authority_lost' AND entity_id=(SELECT action_id FROM admin_transcription_price_publications WHERE price_version=$1)")
       .bind(price_id(&row)).fetch_one(&f.pool).await.expect("single revocation audit");
    assert_eq!(events, 1);
}
