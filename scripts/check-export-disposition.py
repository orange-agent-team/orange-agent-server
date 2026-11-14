#!/usr/bin/env python3
"""Writer quiescence, reversible retirement and recovery-copy acceptance only."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--legacy-runtime',type=Path,required=True);args=p.parse_args()
legacy=args.legacy_runtime.resolve()
if json.loads((legacy/'release-manifest.json').read_text()).get('origin_commit')!='e77a61e2a213b817770e7e01e38a742043df86b6':
    raise SystemExit('Exact fixed schema20/e77a61e source is required')
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
if not subprocess.check_output([str(cargo.parent/'rustc'),'--version'],env=env,text=True).startswith('rustc 1.97.1 '):raise SystemExit('Pinned Rust1.97.1 required')
subprocess.run([str(cargo),'test','--locked','--offline','-p','orange-agent-runtime','engine::tests::editing::retention_export'],cwd=ROOT,env=env,check=True,timeout=300)
subprocess.run([str(cargo),'clippy','--locked','--offline','-p','orange-agent-runtime','--all-targets','--','-D','warnings'],cwd=ROOT,env=env,check=True,timeout=300)
# The driver verifies the legacy manifest digest and every source-file hash,
# exercises actual old CLI-created edit/approval data, and rejects downgrades.
subprocess.run([sys.executable,str(ROOT/'scripts/check-file-editing.py'),'--legacy-runtime',str(legacy)],cwd=ROOT,env=env,check=True,timeout=600)
print('PASS bounded registered-writer quiescence, independently approved retirement/copy restoration, actual retained files, no overwrite, failure/unknown no replay, capsule identity checks and schema20 migration. No permanent erasure, storage/admission release, Desktop, manual picker or other OS certificate.')
