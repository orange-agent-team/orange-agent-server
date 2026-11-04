#!/usr/bin/env python3
"""Actual SQLite/filesystem consent, revocation and status-query behavior."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
subprocess.run([str(cargo),'test','-p','orange-agent-runtime','workspaces::tests','--locked'],cwd=ROOT,env=environment,check=True)
print('PASS actual local workspace consent/SQLite revocation/run summaries; explicit model fixtures, no OS file-picker/bookmark or cloud permission certification')
