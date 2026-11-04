#!/usr/bin/env python3
"""Verify with pinned Rust tools; optional real Postgres tests fail if no DB exists."""
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
channel = os.environ.get('ORANGE_RUST_TOOLCHAIN', 'stable')
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', channel, 'cargo'], text=True).strip())
environment = os.environ.copy()
environment['PATH'] = str(cargo.parent) + os.pathsep + environment.get('PATH', '')
version = subprocess.check_output([str(cargo.parent/'rustc'), '--version'], env=environment, text=True)
if not version.startswith('rustc 1.97.1 '):
    raise SystemExit('Expected Rust 1.97.1; select installed toolchain with ORANGE_RUST_TOOLCHAIN')
def run(*args):
    subprocess.run([str(cargo), *args], cwd=ROOT, env=environment, check=True)
if '--format' in sys.argv:
    run('fmt', '--all')
else:
    run('fmt', '--all', '--check')
    run('test', '--workspace', '--locked')
    run('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    if '--postgres' in sys.argv:
        url = ROOT/'.local/database-url'
        if not environment.get('ORANGE_TEST_DATABASE_URL'):
            if not url.is_file():
                raise SystemExit('Real PostgreSQL required: run scripts/local-postgres.py start first')
            environment['ORANGE_TEST_DATABASE_URL'] = url.read_text().strip()
        run('test', '-p', 'orange-control-plane', '--features', 'integration-tests', '--test', 'postgres', '--locked')
