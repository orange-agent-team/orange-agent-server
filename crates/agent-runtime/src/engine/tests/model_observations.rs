use super::*;

#[tokio::test(start_paused = true)]
async fn runtime_deadline_is_observed_without_replaying_the_uncertain_invocation()
-> Result<(), Box<dyn std::error::Error>> {
    let f = Fixture::new();
    let mut j = f.open();
    f.start(&mut j);
    assert!(j.latest_model_observation("alice", "run-1")?.is_none());
    let gateway = NeverModel {
        called: AtomicUsize::new(0),
    };
    let (_tx, rx) = watch::channel(false);
    let failed = f.advance(&mut j, &gateway, rx.clone()).await?;
    assert_eq!(failed.phase, Phase::RecoveryRequired);
    assert_eq!(failed.failure.as_deref(), Some("model_unknown"));
    let record = j
        .latest_model_observation("alice", "run-1")?
        .expect("actual selected clock branch");
    assert_eq!(record.kind, "call_time_limit");
    assert_eq!(record.wait_limit_ms.as_deref(), Some("120000"));
    assert!(
        record
            .elapsed_ms
            .as_ref()
            .expect("observed elapsed")
            .parse::<u64>()?
            >= 120000
    );
    assert!(j.latest_model_observation("bob", "run-1").is_err());
    drop(j);
    let mut j = f.open();
    assert_eq!(j.latest_model_observation("alice", "run-1")?, Some(record));
    assert_eq!(
        f.advance(&mut j, &gateway, rx).await?.phase,
        Phase::RecoveryRequired
    );
    assert_eq!(gateway.called.load(Ordering::SeqCst), 1);
    j.cancel_agent_run("alice", "run-1")?;
    assert!(j.latest_model_observation("alice", "run-1").is_err());
    Ok(())
}

#[tokio::test]
async fn gateway_refusal_and_journal_write_failure_have_distinct_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    for interrupt in [false, true] {
        let f = Fixture::new();
        let mut j = f.open();
        f.start(&mut j);
        if interrupt {
            j.connection.execute_batch("CREATE TRIGGER fixture_abort BEFORE UPDATE OF phase ON runtime_runs WHEN NEW.phase='recovery_required' BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;
        }
        let (_tx, rx) = watch::channel(false);
        let gateway = ScriptedModel::new(vec![]);
        let result = f.advance(&mut j, &gateway, rx).await;
        if interrupt {
            assert!(result.is_err());
            assert!(j.latest_model_observation("alice", "run-1")?.is_none());
            let n: i64 = j.connection.query_row(
                "SELECT count(*) FROM runtime_invocations WHERE state='dispatched'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(n, 1);
            j.connection.execute_batch("DROP TRIGGER fixture_abort")?;
            drop(j);
            let j = f.open();
            assert_eq!(
                j.latest_model_observation("alice", "run-1")?
                    .expect("reopen evidence")
                    .kind,
                "host_reopened"
            );
        } else {
            assert_eq!(result?.phase, Phase::RecoveryRequired);
            let obs = j
                .latest_model_observation("alice", "run-1")?
                .expect("gateway outcome");
            assert_eq!(obs.kind, "gateway_failed");
            assert_eq!(obs.adapter_code.as_deref(), Some("unclassified"));
            assert!(obs.elapsed_ms.is_some());
        }
    }
    Ok(())
}

#[test]
fn upgrading_a_prior_gateway_observation_does_not_infer_its_adapter_code()
-> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE runtime_invocations(invocation TEXT PRIMARY KEY); INSERT INTO runtime_invocations VALUES('previous'),('next');")?;
    db.execute_batch(include_str!("../../schema_v34.sql"))?;
    db.execute(
        "INSERT INTO runtime_model_observations VALUES('previous','gateway_failed',17,120000,89)",
        [],
    )?;
    db.execute_batch(include_str!("../../schema_v35.sql"))?;
    let old:(String,i64,Option<String>)=db.query_row("SELECT kind,elapsed_ms,adapter_code FROM runtime_model_observations WHERE invocation='previous'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    assert_eq!(old, ("gateway_failed".into(), 89, None));
    assert!(db.execute("UPDATE runtime_model_observations SET adapter_code='invalid' WHERE invocation='previous'",[]).is_err());
    assert!(db.execute("INSERT INTO runtime_model_observations VALUES('next','gateway_failed',18,120000,100,'raw error text')",[]).is_err());
    Ok(())
}
