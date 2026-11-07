#!/usr/bin/env python3
"""Run the existing dev-only user API with an explicit private object root."""
import os
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
private=ROOT/'.local/artifacts'
private.mkdir(mode=0o700,exist_ok=True)
if private.is_symlink() or private.stat().st_mode&0o077:raise SystemExit('Expected a private real .local/artifacts directory')
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
env.update(ORANGE_ENV='development',DATABASE_URL=(ROOT/'.local/database-url').read_text().strip(),ORANGE_USER_CONFIG_FILE=str(ROOT/'.local/identity/user-config.json'),ORANGE_ARTIFACT_ROOT=str(private))
subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-user-api','--locked'],cwd=ROOT,env=env,check=True)
os.execve(ROOT/'target/debug/orange-user-api',[str(ROOT/'target/debug/orange-user-api')],env)
