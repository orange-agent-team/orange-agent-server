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

BASELINE="8ac6b54abc43d744f3d23fb2d83bc307c003af20"
root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
for name,default in [("database-url-file",".local/database-url"),("runtime",".local/model-runtime"),("identity-dir",".local/identity")]:
    parser.add_argument("--"+name,type=Path,default=root/default)
args=parser.parse_args()
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
env.update(ORANGE_TEST_MODEL_RUNTIME_ROOT=str(args.runtime.resolve()),ORANGE_TEST_USER_CONFIG_FILE=str((args.identity_dir/"user-config.json").resolve()),
           ORANGE_TEST_OIDC_CONFIG_FILE=str((args.identity_dir/"admin-config.json").resolve()),CARGO_NET_OFFLINE="true")
env.setdefault("CARGO_TARGET_DIR",str(root/"target"))
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup","which","cargo"],cwd=root,text=True).strip())
env["PATH"]=str(cargo.parent)+os.pathsep+env["PATH"]
with tempfile.TemporaryDirectory(prefix="orange-meter-prior-") as temp, database(args.database_url_file,"orange_meter_upgrade_") as (_,url):
    folder=Path(temp);previous=folder/"source";previous.mkdir()
    archive=folder/"source.tar"
    subprocess.run(["git","archive","--format=tar","--output",archive,BASELINE],cwd=root,check=True)
    subprocess.run(["tar","-xf",archive,"-C",previous],check=True)
    print("Preparing original settled jobs on schema 64 at "+BASELINE,flush=True)
    env["ORANGE_TEST_DATABASE_URL"]=url
    subprocess.run([sys.executable,previous/"scripts/check-trusted-pricing.py"],cwd=previous,env=env,check=True)
    env["ORANGE_METER_UPGRADE_DATABASE"]=url
    subprocess.run([cargo,"test","--offline","--locked","-p","orange-control-plane","--features","metering-upgrade-tests","--test","metering_upgrade","--","--nocapture"],cwd=root,env=env,check=True)
