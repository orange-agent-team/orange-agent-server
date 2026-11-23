#!/usr/bin/env python3
"""Run actual schema-64 pricing, then upgrade that owned DB with the current SQLx migrator."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from owned_postgres import database

BASELINES={64:"8ac6b54abc43d744f3d23fb2d83bc307c003af20",65:"b550ce6b1809dd7a98b956cce07a1960bdc82234"}
root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
for name,default in [("database-url-file",".local/database-url"),("runtime",".local/model-runtime"),("identity-dir",".local/identity")]:
    parser.add_argument("--"+name,type=Path,default=root/default)
parser.add_argument("--earning-upgrade",action="store_true",help="Build real schema-65 jobs and verify original provider entitlements after upgrade")
args=parser.parse_args()
version=65 if args.earning_upgrade else 64
baseline=BASELINES[version]
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
env.update(ORANGE_TEST_MODEL_RUNTIME_ROOT=str(args.runtime.resolve()),ORANGE_TEST_USER_CONFIG_FILE=str((args.identity_dir/"user-config.json").resolve()),
           ORANGE_TEST_OIDC_CONFIG_FILE=str((args.identity_dir/"admin-config.json").resolve()),CARGO_NET_OFFLINE="true")
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup","which","cargo"],cwd=root,text=True).strip())
env["PATH"]=str(cargo.parent)+os.pathsep+env["PATH"]
with tempfile.TemporaryDirectory(prefix="orange-meter-prior-") as temp, database(args.database_url_file,"orange_meter_upgrade_") as (_,url):
    folder=Path(temp);previous=folder/"source";previous.mkdir()
    archive=folder/"source.tar"
    subprocess.run(["git","archive","--format=tar","--output",archive,baseline],cwd=root,check=True)
    subprocess.run(["tar","-xf",archive,"-C",previous],check=True)
    print(f"Preparing original settled jobs on schema {version} at {baseline}",flush=True)
    env["ORANGE_TEST_DATABASE_URL"]=url
    # Same-named local crates at two Git revisions must never share dependency artifacts.
    # Release the prior compilation cache before building the current consumer; keep the DB intact.
    with tempfile.TemporaryDirectory(prefix="schema-prior-target-",dir=folder) as target:
        subprocess.run([sys.executable,previous/"scripts/check-trusted-pricing.py"],cwd=previous,
                       env=dict(env,CARGO_TARGET_DIR=target,CARGO_INCREMENTAL="0"),check=True)
    env["ORANGE_METER_UPGRADE_DATABASE"]=url
    env["ORANGE_METER_PRIOR_VERSION"]=str(version)
    with tempfile.TemporaryDirectory(prefix="schema-current-target-",dir=folder) as target:
        subprocess.run([cargo,"test","--offline","--locked","-p","orange-control-plane","--features","metering-upgrade-tests","--test","metering_upgrade","--","--nocapture"],cwd=root,env=dict(env,CARGO_TARGET_DIR=target,CARGO_INCREMENTAL="0"),check=True)
    print("PASS historical pricing producer and current migration consumer used disjoint clean Cargo targets for schema",version,flush=True)
