#!/usr/bin/env python3
"""Verify user Run egress and provider grants using a disposable PostgreSQL database."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database

root=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument('--database-url-file',required=True,type=Path)
p.add_argument('--identity-config',required=True,type=Path)
args=p.parse_args()
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','1.97.1','cargo'],text=True).strip())
with database(args.database_url_file,'orange_repo_grants_') as (_,url):
    subprocess.run([cargo,'test','--offline','--locked','-p','orange-control-plane','--features','repository-fixture-tests','--test','user_identity','repository_consent::'],
                   cwd=root,check=True,env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ['PATH'],ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve())))
