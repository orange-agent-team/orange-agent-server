#!/usr/bin/env python3
"""Run the trusted metering delivery and retry workflows in an owned development database."""
from acceptance_origins import reserve_origins
import hashlib
import json
import argparse
import os
from pathlib import Path
import subprocess
import sys
from owned_postgres import database

root=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--database-url-file", type=Path, default=root/".local/database-url")
cli.add_argument("--identity-dir", type=Path, default=root/".local/identity")
cli.add_argument("--runtime", type=Path, default=root/".local/model-runtime")
cli.add_argument("--admin-repo", type=Path)
args=cli.parse_args()
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
env.update(ORANGE_TEST_MODEL_RUNTIME_ROOT=str(args.runtime.resolve()),
           ORANGE_TEST_USER_CONFIG_FILE=str((args.identity_dir/"user-config.json").resolve()),
           ORANGE_TEST_OIDC_CONFIG_FILE=str((args.identity_dir/"admin-config.json").resolve()))
if args.admin_repo:
    reserve_origins(43112)
    admin=args.admin_repo.resolve()
    snapshot=json.loads((admin/"docs/contracts/snapshot.json").read_bytes())
    for entry in snapshot["files"].values():
        if hashlib.sha256((root/entry["source"]).read_bytes()).hexdigest()!=entry["sha256"]:
            raise ValueError("Admin snapshot does not match metering contract")
    node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    subprocess.run([node,admin/"scripts/check.mjs"],cwd=admin,check=True)
    subprocess.run([sys.executable,admin/"scripts/local-tls.py"],cwd=admin,check=True)
    evidence=root/".local/metering-recovery";evidence.mkdir(mode=0o700,parents=True,exist_ok=True)
    env.update(ORANGE_METERING_ADMIN_NODE=str(node),ORANGE_METERING_ADMIN_BROWSER=str(admin/"apps/admin/scripts/real-metering-replay.mjs"),
               ORANGE_METERING_SCREENSHOT=str(evidence/"approved-replay.png"))
with database(args.database_url_file,"orange_meter_retry_") as (_,url):
    env["ORANGE_TEST_DATABASE_URL"]=url
    subprocess.run([sys.executable,root/"scripts/check-trusted-pricing.py"],cwd=root,env=env,check=True)
