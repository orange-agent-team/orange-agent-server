#!/usr/bin/env python3
"""Check versioned cancellation and terminal concurrency on a fresh real development database."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument("--environment",type=Path,default=ROOT/".local")
p.add_argument("--section",required=True,choices=["race","cancel","upgrade","policy","interruptions"])
args=p.parse_args()
private=args.environment.resolve()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true")
with database(private/"database-url","orange_job_revision_") as (_,url):
    env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
    if args.section=="policy":
        env["ORANGE_QUARANTINE_HEARTBEAT_TEST"]="1"
        command=[sys.executable,ROOT/"scripts/check-worker-session.py"]
        required={"ordinary_authenticated_heartbeat_preserves_quarantine_and_cannot_recreate_execution"}
    elif args.section=="interruptions":
        command=[sys.executable,ROOT/"scripts/check-queue-expiry.py"]
        required={"expiry_ledger_failure_rolls_back_and_concurrent_user_cancel_refunds_once","offered_expiry_cannot_issue_a_late_start_or_claim_resource_cleanup",
                  "accepted_execution_never_uses_queue_refund_even_if_a_stale_state_is_injected"}
    elif args.section=="cancel":
        env["ORANGE_CANCEL_PROTOCOL_TESTS"]="1"
        command=[sys.executable,ROOT/"scripts/check-worker-session.py"]
        required={"conditional_cancellation_replays_the_original_precondition_but_rejects_new_stale_commands",
                  "running_cancel_keeps_hold_and_attempt_until_independent_cleanup","cancelled_request_rolls_back_when_durable_event_cannot_commit"}
    else:
        case=("existing_schema66_job_and_hold_survive_revision_upgrade" if args.section=="upgrade" else
              "only_one_concurrent_terminal_revision_wins_and_an_old_attempt_cannot_rewind_it")
        command=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features","integration-tests","--test","postgres",case,"--","--exact"]
        required={case}
    with subprocess.Popen(command,env=env,cwd=ROOT,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as child:
        for line in child.stdout:
            print(line,end="",flush=True)
            for case in tuple(required):
                if case+" ... ok" in line:
                    required.remove(case)
        if child.wait() or required:
            raise RuntimeError("Job state gate failed: "+str(sorted(required)))
print("PASS Job revision",args.section,flush=True)
