#!/usr/bin/env python3
"""Tool contract admission, durable capture/approval and real old-version recovery.

Model/identity responses in this command are declared local fixtures. Source-bound
real-model evidence is recorded separately; no production certification implied.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--legacy-runtime', type=Path, required=True)
args = parser.parse_args()
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
if not subprocess.check_output([str(cargo.parent / 'rustc'), '--version'], env=env, text=True).startswith('rustc 1.97.1 '):
    raise SystemExit('Pinned producer Rust1.97.1 required')
for command in [
    [str(cargo), 'test', '-p', 'orange-protocol', 'tool_spec::', '--locked', '--offline'],
    [str(cargo), 'test', '-p', 'orange-agent-runtime', 'catalog::', '--locked', '--offline'],
    [str(cargo), 'test', '-p', 'orange-agent-runtime', 'engine::tests::contracts::', '--locked', '--offline'],
    [sys.executable, str(ROOT / 'scripts/check-file-creation.py'), '--legacy-runtime', str(args.legacy_runtime.resolve())],
]:
    subprocess.run(command, cwd=ROOT, env=env, check=True, timeout=600)
print('PASS bounded offline descriptors and complete snapshot integrity, atomic dispatch/approval pins, exact legacy migration, schema-valid cache corruption rejection and actual old/new CLI recovery. Explicit model/count/local-owner fixtures; not plugin executors, arbitrary parallelism or OS certification')
