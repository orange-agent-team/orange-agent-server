use super::*;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub(super) async fn run(
    pool: &PgPool,
    app: Router,
    principal: &str,
    token: &str,
    a: &str,
    b: &str,
) {
    let Ok(driver) = std::env::var("ORANGE_AUDIT_BROWSER_DRIVER") else {
        return;
    };
    use std::os::unix::fs::PermissionsExt;
    // Keep the global roles/permission aggregate and visible scope set unchanged
    // when only the selected scope's audit grant is later revoked.
    let third = format!("c-{}", Uuid::new_v4());
    for (role, scope) in [("compute_operator", a), ("auditor", third.as_str())] {
        sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,$2,'dev','tenant',$3,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+3600)").bind(principal).bind(role).bind(scope).execute(pool).await.expect("independent per-scope grants");
    }

    sqlx::query("INSERT INTO owners(owner_id) VALUES($1),($2) ON CONFLICT DO NOTHING")
        .bind(a)
        .bind(b)
        .execute(pool)
        .await
        .expect("browser case owners");
    for n in 0..51 {
        sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,business_ref,environment,state,next_check_at) VALUES($1,$2,$3,'dev','investigating',1700000000)").bind(format!("browser-case-{n:03}")).bind(a).bind(format!("browser-business-{n:03}")).execute(pool).await.expect("browser case");
    }
    sqlx::query("INSERT INTO reconciliation_cases(case_id,owner_id,business_ref,environment,state,next_check_at) VALUES('foreign-browser-case',$1,'foreign-hidden','dev','open',1700000000)").bind(b).execute(pool).await.expect("foreign fixture");
    for i in 0..55 {
        seed(pool, &format!("browser-row-{i}"), Some("dev"), Some(a)).await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("owned API listener");
    let address = listener.local_addr().expect("listener address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let evidence = PathBuf::from(
        std::env::var("ORANGE_AUDIT_BROWSER_EVIDENCE").expect("owned evidence directory"),
    );
    let fixture = evidence.join("browser-private.json");
    let bytes=serde_json::to_vec(&serde_json::json!({"api":format!("http://{address}/"),"origin":"https://127.0.0.1:43141","cookie":format!("{SESSION_COOKIE}={token}"),"principal":principal,"scope_a":a,"scope_b":b,"scope_c":third})).expect("fixture JSON");
    let mut options = std::fs::OpenOptions::new();
    use std::os::unix::fs::OpenOptionsExt;
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&fixture).expect("private fixture");
    std::io::Write::write_all(&mut file, &bytes).expect("private fixture write");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o600))
        .expect("private mode");
    let mut browser = tokio::process::Command::new(
        std::env::var("ORANGE_AUDIT_BROWSER_NODE").expect("pinned Node"),
    )
    .arg(driver)
    .arg(&fixture)
    .env_clear()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .expect("owned browser driver");
    let mut output = BufReader::new(browser.stdout.take().expect("driver stdout")).lines();
    let gate = tokio::time::timeout(Duration::from_secs(90), async {
        while let Some(line) = output.next_line().await.expect("driver progress") {
            if line == "AUDIT_READY_FOR_REVOCATION" {
                return true;
            }
        }
        false
    })
    .await;
    if matches!(gate, Ok(true)) {
        let changed=sqlx::query("UPDATE admin_grants SET revoked=true WHERE principal_id=$1 AND environment='dev' AND scope_kind='tenant' AND scope_id=$2 AND role='auditor' AND NOT revoked").bind(principal).bind(a).execute(pool).await.expect("actual role revocation").rows_affected();
        assert_eq!(changed, 1);
        browser
            .stdin
            .as_mut()
            .expect("owned control")
            .write_all(b"revoked\n")
            .await
            .expect("confirm committed revocation");
        browser
            .stdin
            .as_mut()
            .expect("owned control")
            .flush()
            .await
            .expect("flush control");
    }
    let status = match tokio::time::timeout(Duration::from_secs(30), browser.wait()).await {
        Ok(result) => result.expect("browser exit"),
        Err(_) => {
            browser.kill().await.expect("stop only owned driver");
            browser.wait().await.expect("owned driver reaped")
        }
    };
    let mut diagnostic = String::new();
    browser
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut diagnostic)
        .await
        .expect("diagnostic");
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("owned API shutdown")
        .expect("API task")
        .expect("API exit");
    std::fs::remove_file(fixture).expect("remove only generated session fixture");
    assert!(
        matches!(gate, Ok(true)),
        "browser did not reach verified-read gate: {diagnostic}"
    );
    assert!(
        status.success(),
        "actual browser acceptance failed: {diagnostic}"
    );
    let report: Value = serde_json::from_slice(
        &std::fs::read(evidence.join("browser-result.json")).expect("actual browser report"),
    )
    .expect("report");
    for field in [
        "passed",
        "actual_api",
        "legacy_trace_null",
        "scope_specific_permissions",
        "pagination_and_detail",
        "revoked_content_cleared",
        "global_aggregate_unchanged_after_scope_revocation",
        "no_ui_mutations",
        "session_fixture",
    ] {
        assert_eq!(report[field], true, "missing browser evidence: {field}");
    }
    assert_eq!(report["mfa_issuance_certified"], false);
}
