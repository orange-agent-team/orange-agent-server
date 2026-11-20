#!/usr/bin/env python3
"""Run Notion's pinned-protocol fixtures and the real platform authorization test."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--database-url-file",type=Path,required=True)
parser.add_argument("--identity-config",type=Path,required=True)
args = parser.parse_args()
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env = dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
command = [cargo,"test","--locked","--offline","-p","orange-control-plane","--features","notion-fixture-tests"]
subprocess.run(command+["--lib","notion::tests"],cwd=root,env=env,check=True)
with database(args.database_url_file,"orange_notion_") as (_,url):
    subprocess.run(command+["--test","user_identity","notion_consent::"],cwd=root,
                   env=dict(env,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve())),check=True)
