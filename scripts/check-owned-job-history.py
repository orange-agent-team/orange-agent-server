#!/usr/bin/env python3
"""Verify true session/Job authority, bounded durable replay, reset and attempt references."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--environment",type=Path,default=ROOT/".local")
args=parser.parse_args()
private=args.environment.resolve()
subprocess.run([sys.executable,ROOT/"scripts/check-artifact-contract.py"],cwd=ROOT,check=True)
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
with database(private/"database-url","orange_job_events_") as (_,url):
    env.update(ORANGE_JOB_HISTORY_TEST="1",ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
    child=subprocess.Popen([sys.executable,ROOT/"scripts/check-worker-session.py"],env=env,cwd=ROOT,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    found=False
    with child:
        for line in child.stdout:
            print(line,end="",flush=True)
            if "owned_history_resets_after_retention_replays_only_digests_and_binds_real_attempts ... ok" in line:
                found=True
        if child.wait() or not found:
            raise RuntimeError("Real Job history case did not pass")
print("PASS actual owned Job snapshot, replay, reset, tenant fence, mTLS attempt receipt and logout")
