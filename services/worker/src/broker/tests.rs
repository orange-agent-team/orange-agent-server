use super::*;
fn config() -> BrokerConfig {
    BrokerConfig {
        owner_identity: "S-1-test-owner".into(),
        devices: vec!["GPU-test-device-identity".into()],
        approved_profiles: vec![ApprovedProfile {
            id: "approved-text".into(),
            model_sha256: "a".repeat(64),
            execution_digest: "b".repeat(64),
        }],
    }
}
fn directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private");
    }
    dir
}
fn peer(name: &str) -> VerifiedPeer {
    VerifiedPeer::from_verified_os_identity(
        "S-1-test-owner".into(),
        format!("boot-identity-process-start-{name}"),
        name.into(),
        "n".repeat(32),
    )
    .expect("test OS identity substitute")
}
fn request(name: &str, mode: Holder) -> AcquireRequest {
    AcquireRequest {
        request_id: name.into(),
        device_id: "GPU-test-device-identity".into(),
        profile_id: "approved-text".into(),
        mode,
        remote: if mode == Holder::Remote {
            Some(RemoteBinding {
                node_id: "node".into(),
                attempt_id: "attempt-1".into(),
                lease_id: "lease-1".into(),
                connection_epoch: 2,
                fencing_token: 9,
            })
        } else {
            None
        },
    }
}
fn reserved(result: AcquireResult) -> Grant {
    match result {
        AcquireResult::Reserved { grant } => grant,
        other => panic!("expected reserved, got {other:?}"),
    }
}
/// Explicit device probe substitute. Proves core transition logic only.
struct Probe {
    healthy: bool,
    matched: bool,
}
impl ResourceProbe for Probe {
    fn inspect<'a>(&'a self, challenge: &'a ProbeChallenge) -> ProbeFuture<'a> {
        Box::pin(async move {
            Ok(ProbeObservation {
                device_id: challenge.device_id.clone(),
                challenge_nonce: if self.matched {
                    challenge.nonce.clone()
                } else {
                    "old-nonce".into()
                },
                prior_grant_id: challenge.prior_grant.as_ref().map(|g| g.grant_id.clone()),
                all_managed_tasks_joined: true,
                all_managed_processes_exited: true,
                device_probe_available: self.healthy,
                device_healthy: self.healthy,
                evidence_digest: "c".repeat(64),
            })
        })
    }
}
const GOOD: Probe = Probe {
    healthy: true,
    matched: true,
};

#[tokio::test]
async fn contribution_revocation_fences_reserved_grants_without_claiming_cleanup() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("enable");
    let supervisor = broker
        .register_supervisor(peer("remote-owner"))
        .expect("supervisor");
    let other = broker
        .register_supervisor(peer("another-supervisor"))
        .expect("other");
    let req = request("reserved-before-revocation", Holder::Remote);
    let grant = reserved(broker.acquire(&supervisor, &req).expect("reserve"));
    let cloud = VerifiedCloudGrant::from_verified_start(
        req.remote.clone().expect("remote"),
        &grant,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("cloud fixture");
    broker.set_remote_allowed(false).expect("revoke");
    broker.set_remote_allowed(false).expect("repeat revocation");
    assert_eq!(
        broker.slot(&grant.device_id).expect("slot").state,
        "draining"
    );
    assert!(matches!(
        broker.confirm_start(&supervisor, &grant, Some(&cloud)),
        Err(BrokerError::Stale)
    ));
    assert!(matches!(
        broker.acquire(&supervisor, &req),
        Err(BrokerError::Stale)
    ));
    assert_eq!(
        broker.draining_grants(&supervisor).expect("owned drains"),
        vec![grant.clone()]
    );
    assert!(
        broker
            .draining_grants(&other)
            .expect("other cannot consume grant")
            .is_empty()
    );
    let count: i64 = broker
        .connection
        .query_row(
            "SELECT count(*) FROM broker_events WHERE grant_id=? AND kind='grant.draining'",
            [&grant.grant_id],
            |r| r.get(0),
        )
        .expect("audit");
    assert_eq!(count, 1);
    broker.set_remote_allowed(true).expect("new consent");
    assert!(
        matches!(
            broker.confirm_start(&supervisor, &grant, Some(&cloud)),
            Err(BrokerError::Stale)
        ),
        "new consent cannot resurrect an old grant"
    );
    assert_eq!(
        broker.slot(&grant.device_id).expect("still occupied").state,
        "draining"
    );
    broker
        .release(&supervisor, &grant, &GOOD)
        .await
        .expect("explicit probe substitute");
    assert!(
        broker
            .draining_grants(&supervisor)
            .expect("drained")
            .is_empty()
    );
    assert_eq!(
        broker.slot(&grant.device_id).expect("released").state,
        "idle"
    );
}

#[tokio::test]
async fn contribution_revocation_drains_running_remote_and_preserves_local_work() {
    let dir = directory();
    let mut configuration = config();
    configuration.devices.push("second-device".into());
    let mut broker =
        Broker::open(&dir.path().join("broker.sqlite"), configuration.clone()).expect("open");
    for device in &configuration.devices {
        broker.reconcile(device, &GOOD).await.expect("test probe");
    }
    let local = broker
        .register_supervisor(peer("local-owner"))
        .expect("local");
    let remote = broker
        .register_supervisor(peer("remote-owner"))
        .expect("remote");
    let mut local_request = request("local-before-revocation", Holder::Local);
    local_request.device_id = "second-device".into();
    let local_grant = reserved(
        broker
            .acquire(&local, &local_request)
            .expect("local reserve"),
    );
    broker
        .confirm_start(&local, &local_grant, None)
        .expect("local start");
    broker.set_remote_allowed(true).expect("enable");
    let req = request("remote-before-revocation", Holder::Remote);
    let grant = reserved(broker.acquire(&remote, &req).expect("remote reserve"));
    let cloud = VerifiedCloudGrant::from_verified_start(
        req.remote.expect("remote"),
        &grant,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("cloud fixture");
    broker
        .confirm_start(&remote, &grant, Some(&cloud))
        .expect("remote start");
    broker.set_remote_allowed(false).expect("revoke");
    assert_eq!(
        broker.slot(&grant.device_id).expect("remote slot").state,
        "draining"
    );
    assert!(matches!(
        broker.renew(&remote, &grant, None),
        Err(BrokerError::Stale)
    ));
    assert_eq!(
        broker
            .slot(&local_grant.device_id)
            .expect("local slot")
            .state,
        "running_local"
    );
    broker
        .renew(&local, &local_grant, None)
        .expect("local unaffected");
    assert_eq!(
        broker.draining_grants(&remote).expect("stop list"),
        vec![grant.clone()]
    );
    assert!(
        broker
            .draining_grants(&local)
            .expect("local stop list")
            .is_empty()
    );
    drop(broker);
    let restarted =
        Broker::open(&dir.path().join("broker.sqlite"), configuration).expect("restart");
    assert_eq!(
        restarted
            .slot(&grant.device_id)
            .expect("remote reconciliation")
            .state,
        "reconciling"
    );
    let allowed: bool = restarted
        .connection
        .query_row("SELECT remote_allowed FROM broker_meta", [], |r| r.get(0))
        .expect("persisted consent");
    assert!(!allowed);
    assert!(matches!(
        restarted.draining_grants(&remote),
        Err(BrokerError::Unauthorized)
    ));
}

#[tokio::test]
async fn contribution_revocation_rolls_back_if_durable_evidence_cannot_commit() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("enable");
    let remote = broker
        .register_supervisor(peer("remote-owner"))
        .expect("remote");
    let req = request("reserve", Holder::Remote);
    let grant = reserved(broker.acquire(&remote, &req).expect("reserve"));
    broker.connection.execute_batch("CREATE TEMP TRIGGER deny_revocation_audit BEFORE INSERT ON broker_events WHEN NEW.kind='contribution.policy_changed' BEGIN SELECT RAISE(ABORT,'test audit storage fault'); END;").expect("fault injection");
    assert!(broker.set_remote_allowed(false).is_err());
    assert_eq!(
        broker
            .slot(&grant.device_id)
            .expect("atomic rollback")
            .state,
        "reserved_remote"
    );
    assert!(
        broker
            .draining_grants(&remote)
            .expect("no partial drain")
            .is_empty()
    );
    let allowed: bool = broker
        .connection
        .query_row("SELECT remote_allowed FROM broker_meta", [], |r| r.get(0))
        .expect("policy rolled back");
    assert!(allowed);
    broker
        .connection
        .execute_batch("DROP TRIGGER deny_revocation_audit;")
        .expect("remove fixture fault");
    broker
        .set_remote_allowed(false)
        .expect("retry succeeds after storage recovery");
    assert_eq!(
        broker.draining_grants(&remote).expect("drain durable"),
        vec![grant]
    );
}
async fn initialized(dir: &tempfile::TempDir) -> Broker {
    let mut broker = Broker::open(&dir.path().join("broker.sqlite"), config()).expect("open");
    broker
        .reconcile("GPU-test-device-identity", &GOOD)
        .await
        .expect("test probe");
    broker
}
#[tokio::test]
async fn singleton_persistent_arbitration_and_exact_request_replay() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    assert!(matches!(
        Broker::open(&dir.path().join("broker.sqlite"), config()),
        Err(BrokerError::Busy)
    ));
    let a = broker.register_supervisor(peer("supervisor-a")).expect("a");
    let b = broker.register_supervisor(peer("supervisor-b")).expect("b");
    let req = request("first", Holder::Local);
    let grant = reserved(broker.acquire(&a, &req).expect("reserve"));
    assert_eq!(
        broker.acquire(&a, &req).expect("same"),
        AcquireResult::Reserved {
            grant: grant.clone()
        }
    );
    assert_eq!(
        broker
            .acquire(&b, &request("second", Holder::Local))
            .expect("compete"),
        AcquireResult::Busy
    );
    assert!(matches!(
        broker.acquire(&b, &req),
        Err(BrokerError::Conflict)
    ));
    let mut changed = req.clone();
    changed.mode = Holder::Remote;
    changed.remote = request("x", Holder::Remote).remote;
    assert!(matches!(
        broker.acquire(&a, &changed),
        Err(BrokerError::Conflict)
    ));
    assert!(broker.confirm_start(&b, &grant, None).is_err());
    broker
        .confirm_start(&a, &grant, None)
        .expect("start barrier");
    assert!(
        broker.confirm_start(&a, &grant, None).is_err(),
        "start is not a repeatable execution permission"
    );
}
#[tokio::test]
async fn remote_ack_does_not_start_and_local_preempts_unstarted_remote() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("policy");
    let remote = broker.register_supervisor(peer("remote")).expect("remote");
    let local = broker.register_supervisor(peer("local")).expect("local");
    let req = request("remote-request", Holder::Remote);
    let old = reserved(broker.acquire(&remote, &req).expect("reserve"));
    assert!(matches!(
        broker.confirm_start(&remote, &old, None),
        Err(BrokerError::CloudGrantRequired)
    ));
    let new = reserved(
        broker
            .acquire(&local, &request("local-request", Holder::Local))
            .expect("preempt"),
    );
    assert!(new.slot_generation > old.slot_generation);
    let cloud = VerifiedCloudGrant::from_verified_start(
        req.remote.expect("binding"),
        &old,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("test cloud grant");
    assert!(broker.confirm_start(&remote, &old, Some(&cloud)).is_err());
    broker.confirm_start(&local, &new, None).expect("local");
    assert!(broker.begin_drain(&remote, &old).is_err());
}
#[tokio::test]
async fn running_remote_requires_actual_probe_boundary_before_local_reuse() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("policy");
    let remote = broker.register_supervisor(peer("remote")).expect("remote");
    let local = broker.register_supervisor(peer("local")).expect("local");
    let req = request("remote-request", Holder::Remote);
    let old = reserved(broker.acquire(&remote, &req).expect("reserve"));
    let cloud = VerifiedCloudGrant::from_verified_start(
        req.remote.expect("binding"),
        &old,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("test cloud grant");
    broker
        .confirm_start(&remote, &old, Some(&cloud))
        .expect("start");
    assert_eq!(
        broker
            .acquire(&local, &request("local-request", Holder::Local))
            .expect("drain"),
        AcquireResult::DrainRequired { grant: old.clone() }
    );
    assert!(broker.renew(&remote, &old, Some(&cloud)).is_err());
    assert!(
        broker
            .release(
                &remote,
                &old,
                &Probe {
                    healthy: false,
                    matched: true
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        broker.slot(&old.device_id).expect("slot").state,
        "quarantined"
    );
    assert_eq!(
        broker
            .acquire(&local, &request("local-2", Holder::Local))
            .expect("blocked"),
        AcquireResult::RecoveryRequired
    );
    broker
        .reconcile(&old.device_id, &GOOD)
        .await
        .expect("probe recovered");
    let new = reserved(
        broker
            .acquire(&local, &request("local-3", Holder::Local))
            .expect("new"),
    );
    assert!(broker.release(&remote, &old, &GOOD).await.is_err());
    assert_eq!(broker.slot(&new.device_id).expect("slot").grant, Some(new));
    assert!(matches!(
        broker.acquire(&remote, &request("remote-2", Holder::Remote)),
        Err(BrokerError::RemoteDisabled)
    ));
}
#[tokio::test]
async fn restart_invalidates_old_epoch_and_never_assumes_idle() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    let old = reserved(
        broker
            .acquire(&supervisor, &request("one", Holder::Local))
            .expect("grant"),
    );
    broker
        .confirm_start(&supervisor, &old, None)
        .expect("start");
    drop(broker);
    let mut broker = Broker::open(&dir.path().join("broker.sqlite"), config()).expect("restart");
    let slot = broker.slot(&old.device_id).expect("slot");
    assert_eq!(slot.state, "reconciling");
    assert!(slot.broker_epoch > old.broker_epoch);
    assert!(broker.renew(&supervisor, &old, None).is_err());
    let new_supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("new registration");
    assert_eq!(
        broker
            .acquire(&new_supervisor, &request("two", Holder::Local))
            .expect("blocked"),
        AcquireResult::RecoveryRequired
    );
    assert!(
        broker
            .reconcile(
                &old.device_id,
                &Probe {
                    healthy: true,
                    matched: false
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        broker.slot(&old.device_id).expect("slot").state,
        "quarantined"
    );
    broker
        .reconcile(&old.device_id, &GOOD)
        .await
        .expect("reconciled");
    let new = reserved(
        broker
            .acquire(&new_supervisor, &request("three", Holder::Local))
            .expect("new grant"),
    );
    assert!(new.slot_generation > old.slot_generation);
    assert!(broker.confirm_start(&new_supervisor, &old, None).is_err());
}
#[tokio::test]
async fn elapsed_lease_cannot_renew_or_release_without_cleanup() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    let grant = reserved(
        broker
            .acquire(&supervisor, &request("one", Holder::Local))
            .expect("grant"),
    );
    broker
        .confirm_start(&supervisor, &grant, None)
        .expect("start");
    // Explicit monotonic-time simulation, not a real hardware sleep/driver test.
    broker.started -= Duration::from_secs(16);
    assert!(matches!(
        broker.renew(&supervisor, &grant, None),
        Err(BrokerError::Stale)
    ));
    assert_eq!(broker.expire().expect("expired"), vec![grant.clone()]);
    assert_eq!(
        broker.slot(&grant.device_id).expect("slot").state,
        "draining"
    );
    assert_eq!(
        broker
            .acquire(&supervisor, &request("two", Holder::Local))
            .expect("no reuse"),
        AcquireResult::DrainRequired {
            grant: grant.clone()
        }
    );
    broker
        .release(&supervisor, &grant, &GOOD)
        .await
        .expect("probe");
    assert_eq!(broker.slot(&grant.device_id).expect("slot").state, "idle");
}
#[tokio::test]
async fn cloud_fences_and_conservative_send_time_cannot_be_extended_by_reception() {
    let binding = request("r", Holder::Remote).remote.expect("remote");
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("policy");
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    let grant = reserved(
        broker
            .acquire(&supervisor, &request("one", Holder::Remote))
            .expect("grant"),
    );
    let mut wrong = binding.clone();
    assert!(
        VerifiedCloudGrant::from_verified_start(
            binding.clone(),
            &grant,
            Instant::now() - Duration::from_secs(45),
            Duration::from_secs(45)
        )
        .is_err()
    );
    let mut local_ack = grant.clone();
    local_ack.mode = Holder::Local;
    assert!(matches!(
        VerifiedCloudGrant::from_verified_start(
            binding.clone(),
            &local_ack,
            Instant::now(),
            Duration::from_secs(45)
        ),
        Err(BrokerError::Invalid)
    ));
    wrong.connection_epoch += 1;
    let bad = VerifiedCloudGrant::from_verified_start(
        wrong,
        &grant,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("mismatched");
    assert!(matches!(
        broker.confirm_start(&supervisor, &grant, Some(&bad)),
        Err(BrokerError::CloudGrantRequired)
    ));
    let short = VerifiedCloudGrant::from_verified_start(
        binding,
        &grant,
        Instant::now(),
        Duration::from_secs(5),
    )
    .expect("short");
    broker
        .confirm_start(&supervisor, &grant, Some(&short))
        .expect("start");
    broker.renew(&supervisor, &grant, None).expect("heartbeat");
    let (expires, remote): (i64, i64) = broker
        .connection
        .query_row(
            "SELECT expires_ms,remote_deadline_ms FROM grants WHERE grant_id=?",
            [&grant.grant_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("deadlines");
    assert_eq!(expires, remote);
    assert!(remote < 5000, "network response receipt adds no time");
}

#[tokio::test]
async fn cloud_start_permission_cannot_authorize_a_reused_slot() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    broker.set_remote_allowed(true).expect("enable");
    let supervisor = broker
        .register_supervisor(peer("remote-owner"))
        .expect("supervisor");
    let first_request = request("first-reservation", Holder::Remote);
    let first = reserved(broker.acquire(&supervisor, &first_request).expect("first"));
    let first_cloud = VerifiedCloudGrant::from_verified_start(
        first_request.remote.expect("remote"),
        &first,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("verified original ACK fixture");
    broker
        .confirm_start(&supervisor, &first, Some(&first_cloud))
        .expect("first start");
    broker.begin_drain(&supervisor, &first).expect("drain");
    broker
        .release(&supervisor, &first, &GOOD)
        .await
        .expect("explicit cleanup probe fixture");
    let second_request = request("second-reservation-same-cloud-lease", Holder::Remote);
    let second = reserved(
        broker
            .acquire(&supervisor, &second_request)
            .expect("new local reservation"),
    );
    assert_ne!(first.grant_id, second.grant_id);
    assert!(second.slot_generation > first.slot_generation);
    assert!(
        matches!(
            broker.confirm_start(&supervisor, &second, Some(&first_cloud)),
            Err(BrokerError::CloudGrantRequired)
        ),
        "a verified cloud grant for the original ACK must not authorize a different local grant"
    );
    let second_cloud = VerifiedCloudGrant::from_verified_start(
        second_request.remote.expect("remote"),
        &second,
        Instant::now(),
        Duration::from_secs(45),
    )
    .expect("separately verified new ACK fixture");
    broker
        .confirm_start(&supervisor, &second, Some(&second_cloud))
        .expect("new exact grant");
    assert!(
        matches!(
            broker.renew(&supervisor, &second, Some(&first_cloud)),
            Err(BrokerError::CloudGrantRequired)
        ),
        "old cloud permission cannot renew a reused local slot"
    );
}
#[tokio::test]
async fn authentication_profiles_and_future_storage_fail_closed() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    let other = VerifiedPeer::from_verified_os_identity(
        "other-owner".into(),
        "process-start-identity".into(),
        "other-instance".into(),
        "n".repeat(32),
    )
    .expect("peer");
    assert!(matches!(
        broker.register_supervisor(other),
        Err(BrokerError::Unauthorized)
    ));
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    let mut req = request("one", Holder::Local);
    req.profile_id = "arbitrary-command".into();
    assert!(broker.acquire(&supervisor, &req).is_err());
    drop(broker);
    let path = dir.path().join("broker.sqlite");
    let db = Connection::open(&path).expect("db");
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA user_version=2;")
        .expect("future");
    drop(db);
    let bytes = std::fs::read(&path).expect("bytes");
    assert!(matches!(
        Broker::open(&path, config()),
        Err(BrokerError::StoragePolicy)
    ));
    assert_eq!(std::fs::read(path).expect("bytes"), bytes);
}
struct PendingProbe;
impl ResourceProbe for PendingProbe {
    fn inspect<'a>(&'a self, _: &'a ProbeChallenge) -> ProbeFuture<'a> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn dropping_probe_leaves_persisted_quarantine() {
    let dir = directory();
    let mut broker = Broker::open(&dir.path().join("broker.sqlite"), config()).expect("open");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            broker.reconcile("GPU-test-device-identity", &PendingProbe)
        )
        .await
        .is_err()
    );
    assert_eq!(
        broker.slot("GPU-test-device-identity").expect("slot").state,
        "quarantined"
    );
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    assert_eq!(
        broker
            .acquire(&supervisor, &request("one", Holder::Local))
            .expect("blocked"),
        AcquireResult::RecoveryRequired
    );
}

#[tokio::test]
async fn simultaneous_acquire_has_exactly_one_winner() {
    use std::sync::{Arc, Barrier, Mutex};
    let dir = directory();
    let mut broker = initialized(&dir).await;
    let a = broker.register_supervisor(peer("a")).expect("a");
    let b = broker.register_supervisor(peer("b")).expect("b");
    let broker = Arc::new(Mutex::new(broker));
    let barrier = Arc::new(Barrier::new(2));
    let handles = [("a", a), ("b", b)]
        .into_iter()
        .map(|(id, supervisor)| {
            let broker = broker.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                broker
                    .lock()
                    .expect("lock")
                    .acquire(&supervisor, &request(id, Holder::Local))
                    .expect("acquire")
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, AcquireResult::Reserved { .. }))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, AcquireResult::Busy))
            .count(),
        1
    );
}
#[tokio::test]
async fn sleep_reset_invalidates_all_old_authority_and_audit_is_immutable() {
    let dir = directory();
    let mut broker = initialized(&dir).await;
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("supervisor");
    let grant = reserved(
        broker
            .acquire(&supervisor, &request("one", Holder::Local))
            .expect("grant"),
    );
    broker
        .confirm_start(&supervisor, &grant, None)
        .expect("start");
    broker
        .invalidate(Invalidation::ResumeFromSleep)
        .expect("invalidate");
    assert!(broker.renew(&supervisor, &grant, None).is_err());
    assert_eq!(
        broker.slot(&grant.device_id).expect("slot").state,
        "reconciling"
    );
    assert!(
        broker
            .connection
            .execute("DELETE FROM broker_events", [])
            .is_err()
    );
    assert!(
        broker
            .connection
            .execute("UPDATE broker_commands SET digest='changed'", [])
            .is_err()
    );
    let supervisor = broker
        .register_supervisor(peer("supervisor"))
        .expect("register");
    broker
        .reconcile(&grant.device_id, &GOOD)
        .await
        .expect("probe");
    let new = reserved(
        broker
            .acquire(&supervisor, &request("two", Holder::Local))
            .expect("new"),
    );
    assert!(new.broker_epoch > grant.broker_epoch);
    assert!(broker.release(&supervisor, &grant, &GOOD).await.is_err());
}
#[cfg(unix)]
#[test]
fn nonprivate_storage_and_links_cannot_redirect_broker_writes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = directory();
    let target_dir = directory();
    let target = target_dir.path().join("keep");
    std::fs::write(&target, "keep").expect("target");
    let path = dir.path().join("broker.sqlite");
    symlink(&target, &path).expect("link");
    assert!(Broker::open(&path, config()).is_err());
    std::fs::remove_file(&path).expect("remove");
    std::fs::hard_link(&target, &path).expect("link");
    assert!(Broker::open(&path, config()).is_err());
    std::fs::remove_file(&path).expect("remove");
    symlink(&target, dir.path().join("broker.sqlite-wal")).expect("sidecar");
    assert!(Broker::open(&path, config()).is_err());
    std::fs::remove_file(dir.path().join("broker.sqlite-wal")).expect("remove");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("mode");
    assert!(Broker::open(&path, config()).is_err());
    assert_eq!(std::fs::read_to_string(target).expect("unchanged"), "keep");
}
