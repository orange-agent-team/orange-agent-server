#!/usr/bin/env python3
"""Filesystem and deterministic network-fault tests; not an official artifact download certificate."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
subprocess.run([str(cargo),'test','-p','orange-model-runtime','cache::','--locked'],cwd=ROOT,env=environment,check=True)
print('PASS actual local-file import/private cache/lock/integrity and simulated network interruptions; real artifact download/inference is separate')
