#!/usr/bin/env python3
"""Exercise the ordinary HTTP Job route with confirmed PostgreSQL rollbacks on a disposable local database."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--environment",type=Path,default=ROOT/".local")
cli.add_argument("--full",action="store_true",help="Run the entire original execution receiver gate")
a=cli.parse_args();private=a.environment.resolve()
with database(private/"database-url","orange_submit_serial_") as (_,url):
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"),RUST_TEST_THREADS="2",CARGO_NET_OFFLINE="true")
    if not a.full:env["ORANGE_SUBMIT_ROLLBACK_TEST"]="1"
    required={"transient_submit_conflicts_replay_only_confirmed_rollbacks_under_one_key ... ok"}
    if a.full:required.update({"expiry_before_barrier_commit_rolls_back_the_entire_stop ... ok","PASS E08 durable receive/reserve/ACK/start-confirm"})
    cmd="check-execution-receiver.py" if a.full else "check-worker-session.py"
    with subprocess.Popen([sys.executable,ROOT/"scripts"/cmd],cwd=ROOT,env=env,stdout=subprocess.PIPE,text=True) as child:
        for line in child.stdout:
            print(line,end="",flush=True);required.difference_update([r for r in required if r in line])
        if child.wait() or required:raise RuntimeError("Missing complete PostgreSQL acceptance")
print("PASS original HTTP Job submission survives confirmed transient rollbacks with exactly one financial reservation and bounded persistent failure")
