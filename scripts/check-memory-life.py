#!/usr/bin/env python3
"""Exercise real Runtime storage, approvals, revocation, recovery and migration."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--legacy-runtime', required=True, type=Path,
                    help='Fixed schema27 source that produced the historical user-confirmation defect')
args = parser.parse_args()
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
environment = os.environ.copy()
environment['PATH'] = str(cargo.parent) + os.pathsep + environment.get('PATH', '')
if not subprocess.check_output([str(cargo.parent/'rustc'), '--version'], text=True).startswith('rustc 1.97.1 '):
    raise SystemExit('Pinned Runtime Rust 1.97.1 required')
for arguments in (
        ['test', '--locked', '--offline', '-p', 'orange-agent-runtime'],
        ['clippy', '--locked', '--offline', '-p', 'orange-agent-runtime', '--all-targets', '--', '-D', 'warnings']):
    subprocess.run([str(cargo), *arguments], cwd=ROOT, env=environment, check=True, timeout=600)
subprocess.run([sys.executable, str(ROOT/'scripts/check-memory-compatibility.py'),
                '--legacy-runtime', str(args.legacy_runtime.resolve()), '--continuity'],
               cwd=ROOT, env=environment, check=True, timeout=600)
