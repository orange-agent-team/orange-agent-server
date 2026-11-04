#!/usr/bin/env python3
"""Actual workspace filesystem/capability tests. Not a full command/OS sandbox certificate."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
subprocess.run([str(cargo),'test','-p','orange-agent-runtime','workspace::tests','--locked'],cwd=ROOT,env=environment,check=True)
print('PASS actual workspace read/list limits, identity, links, protected paths, pagination and revocation; no full OS/process sandbox claim')
