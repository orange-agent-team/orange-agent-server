#!/usr/bin/env python3
"""Negotiated control receipt: actual SQLite, PostgreSQL and mTLS; no cleanup claim."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
for command in [
    [str(cargo),'test','-p','orange-protocol','-p','orange-worker','--locked'],
    [sys.executable,str(ROOT/'scripts/check-worker-session.py')],
    [str(cargo),'clippy','-p','orange-worker','-p','orange-control-plane','--all-targets','--features','integration-tests','--locked','--','-D','warnings'],
]:
    subprocess.run(command,cwd=ROOT,env=env,check=True)
print('PASS negotiated real mTLS/PostgreSQL/SQLite node receipt and stale/unknown boundaries; explicit accepted-Admin-intent and certificate fixtures, no Broker apply, process/GPU cleanup, node resume or executed model Job')
