#!/usr/bin/env python3
"""Actual SQLite Broker arbitration, with explicit OS/cloud/probe substitutes."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
subprocess.run([str(cargo),'test','-p','orange-worker','--locked'],cwd=ROOT,env=environment,check=True)
print('PASS actual SQLite lock/state/fencing/recovery; authenticated peer/cloud/device-probe fixtures only, no Windows IPC or GPU cleanup certificate')
