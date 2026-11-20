#!/usr/bin/env python3
"""Actual PostgreSQL action/approval transactions with explicit session fixtures.
OIDC discovery is real; this script does not claim issuance of MFA or device cleanup.
"""
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
env = os.environ.copy()
env['PATH'] = str(cargo.parent) + os.pathsep + env.get('PATH', '')
env['ORANGE_TEST_DATABASE_URL'] = (ROOT/'.local/database-url').read_text().strip()
env['ORANGE_TEST_OIDC_CONFIG_FILE'] = str(ROOT/'.local/identity/admin-config.json')
for args in [
    ['test','-p','orange-control-plane','--lib','--locked','--offline','policy_pairs'],
    ['test', '-p', 'orange-control-plane', '--features', 'integration-tests', '--test', 'admin_actions', '--test', 'admin_identity', '--locked'],
    ['clippy', '-p', 'orange-control-plane', '--features', 'integration-tests', '--test', 'admin_actions', '--locked', '--', '-D', 'warnings'],
]:
    subprocess.run([str(cargo), *args], cwd=ROOT, env=env, check=True)
print('PASS actual PostgreSQL administrative intent, independent approval, authorization and durable command enqueue; session fixtures, no claimed device completion')
