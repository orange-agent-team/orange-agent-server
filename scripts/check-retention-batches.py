#!/usr/bin/env python3
"""Verify event retention, pending inbox independence and protected artifact references in actual Postgres."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--environment",type=Path,default=ROOT/".local")
args=parser.parse_args()
private=args.environment.resolve()
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup",
    "which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
cases=[(["--test","events"],"batch_retention_is_bounded_contiguous_concurrent_and_preserves_legacy_atomicity"),
       (["--test","events"],"retention_cannot_advance_over_a_missing_notification_or_change_ledger_postings"),
       (["--test","queue"],"delivered_event_retention_preserves_pending_inbox_and_last_expired_attempt_is_recorded"),
       (["--lib"],"artifacts::tests::active_reference_survives_deletion_retention_and_cannot_be_added_after_delete"),
       (["--lib"],"artifacts::tests::dropping_request_retains_owned_io_and_original_id_recovers_outcome")]
with database(private/"database-url","orange_retention_") as (_,url):
    env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"))
    for target,name in cases:
        result=subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","integration-tests",*target,name,"--","--exact"],
            cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
        print(result.stdout,flush=True)
        result.check_returncode()
        if "test "+name+" ... ok" not in result.stdout:
            raise ValueError("Missing actual retention assertion: "+name)
print("PASS bounded concurrent retention and independent inbox, ledger and artifact ownership")
