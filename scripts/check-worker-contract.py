#!/usr/bin/env python3
"""Run the original Worker protocol gates with real mTLS, a private database and explicit fixtures."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--section",choices=["handshake","execution","renewal","roundtrip"],required=True)
cli.add_argument("--environment",type=Path,default=ROOT/".local")
args=cli.parse_args()
private=args.environment.resolve()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
base={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
base.update(PATH=str(cargo.parent)+os.pathsep+base["PATH"],CARGO_NET_OFFLINE="true",ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
            ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"),ORANGE_TEST_MODEL_RUNTIME_ROOT=str(private/"model-runtime"))

def verify(command,env,cases):
    pending=set(cases)
    with subprocess.Popen(command,cwd=ROOT,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as child:
        for line in child.stdout:
            print(line,end="",flush=True)
            if line.startswith("test ") and line.rstrip().endswith(" ... ok"):
                for name in tuple(pending):
                    if name+" ... ok" in line:pending.remove(name)
        if child.wait() or pending:raise ValueError("Worker acceptance failed; missing exact cases: "+str(sorted(pending)))

def exercise(script,flags,cases):
    with database(private/"database-url","orange_worker_contract_") as (_,url):
        verify([sys.executable,ROOT/"scripts"/script],dict(base,ORANGE_TEST_DATABASE_URL=url,**flags),cases)

if args.section=="handshake":
    verify([cargo,"test","--lib","--locked","--offline","-p","orange-protocol","--features","grpc","worker_capabilities::"],base,
           ["every_v1_capability_combination_has_consistent_provider_and_gateway_semantics","future_major_and_silent_capability_changes_are_rejected"])
    exercise("check-worker-session.py",{"ORANGE_CAPABILITY_OFFER_TEST":"1"},["rejected_capability_offers_do_not_rotate_the_current_device_session"])
elif args.section=="execution":
    exercise("check-worker-session.py",{"ORANGE_EXECUTION_BARRIER_TESTS":"1"},["execution_assignment_replay_and_exact_ack_grant_are_durable",
             "execution_ack_cannot_cross_device_or_connection_epoch","execution_reviewed_profile_revocation_blocks_start","execution_revalidates_the_original_submitted_job_digest"])
elif args.section=="renewal":
    exercise("check-worker-session.py",{"ORANGE_WORKER_RENEWAL_TESTS":"1"},["renewal_is_durable_exact_and_replay_never_adds_time_or_events",
             "expired_execution_is_not_revived_by_current_session","database_wait_crossing_prior_expiry_rolls_back_renewal_instead_of_reviving"])
    exercise("check-real-worker.py",{"ORANGE_UNAVAILABLE_RENEWAL_TEST":"1"},["actual_lost_renewal_response_stops_owned_cpu_before_any_result_is_published"])
else:
    exercise("check-worker-session.py",{"ORANGE_WORKER_OUTPUT_TESTS":"1"},["exact_output_bytes_commit_once_are_user_readable_and_never_settle_money",
              "malformed_chunk_is_invisible_and_exact_retry_preserves_binding","concurrent_identical_uploads_have_one_commit_and_one_stored_transfer"])
    exercise("check-worker-session.py",{"ORANGE_CANCEL_PROTOCOL_TESTS":"1"},["running_cancel_keeps_hold_and_attempt_until_independent_cleanup"])
    exercise("check-worker-session.py",{"ORANGE_EXPIRED_ATTEMPT_TEST":"1"},["execution_heartbeat_does_not_renew_and_expired_start_cannot_be_retrieved"])
print("PASS Worker contract",args.section,flush=True)
