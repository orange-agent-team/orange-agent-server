#!/usr/bin/env python3
"""Real local PostgreSQL and private OpenDAL filesystem; identity fixtures explicit."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
env['ORANGE_TEST_DATABASE_URL']=(ROOT/'.local/database-url').read_text().strip()
env['ORANGE_TEST_USER_CONFIG_FILE']=str(ROOT/'.local/identity/user-config.json')
subprocess.run([os.sys.executable,str(ROOT/'scripts/check-artifact-contract.py')],cwd=ROOT,check=True)
subprocess.run([str(cargo),'test','-p','orange-control-plane','--features','integration-tests','--lib','artifacts::tests','--locked'],cwd=ROOT,env=env,check=True)
print('PASS real PostgreSQL/OpenDAL/private filesystem object slice; explicit session fixtures, no Worker delivery or cloud-storage certification')

subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-user-api','--locked'],cwd=ROOT,env=env,check=True)
subprocess.run([os.sys.executable,str(ROOT/'scripts/artifact-api-smoke.py')],cwd=ROOT,env=env,check=True)
