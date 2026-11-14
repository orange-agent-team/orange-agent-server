#!/usr/bin/env python3
"""Bounded Runtime export and real prior-format migration, not Desktop certification."""
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
if json.loads((legacy/'release-manifest.json').read_text()).get('origin_commit')!='a2cdafb95fab90bc1ded82d4a4c061be40463d35':
    raise SystemExit('This slice requires fixed schema17/a2cdafb migration evidence')
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
if not subprocess.check_output([str(cargo.parent/'rustc'),'--version'],env=env,text=True).startswith('rustc 1.97.1 '):raise SystemExit('Pinned Rust1.97.1 required')
for test in ['engine::tests::editing::retention_scopes','engine::tests::editing::retention_export']:
    subprocess.run([str(cargo),'test','--locked','--offline','-p','orange-agent-runtime',test],cwd=ROOT,env=env,check=True,timeout=300)
subprocess.run([str(cargo),'clippy','--locked','--offline','-p','orange-agent-runtime','--all-targets','--','-D','warnings'],cwd=ROOT,env=env,check=True,timeout=300)
# This independently checks the exact old manifest and complete source inventory.
subprocess.run([sys.executable,str(ROOT/'scripts/check-file-editing.py'),'--legacy-runtime',str(legacy)],cwd=ROOT,env=env,check=True,timeout=600)
print('PASS actual bounded export capture, owned metadata discovery, exact approval, exclusive publication, revoked leases, no overwrite, ambiguous receipts/restart no replay and real schema17 migration plus nonmutating old-version rejection; no Desktop/manual-picker/otherOS/disposal certificate')
