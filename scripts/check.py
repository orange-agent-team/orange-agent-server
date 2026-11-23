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
def run(*args, env=None):
    subprocess.run([str(cargo), *args], cwd=ROOT, env=environment if env is None else env, check=True)
options=set(sys.argv[1:])
focused='--focused' in options
targets={'--http','--postgres','--teams','--events','--queue'}
if options- (targets|{'--format','--focused'}):raise SystemExit('Unknown verification option')
if focused and (not options & targets or '--format' in options):raise SystemExit('--focused requires an explicit integration target and cannot format source')
# The execution DAG independently verifies DOMAIN-001 for these exact sources.
# Standalone/default invocations retain the full unit/Clippy suite.
if '--format' in sys.argv:
    run('fmt', '--all')
else:
    run('fmt', '--all', '--check')
    if not focused:
        run('test', '--workspace', '--locked')
        run('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    if '--http' in sys.argv:
        run('build', '--bin', 'orange-api', '--locked')
        subprocess.run([sys.executable,str(ROOT/'scripts/http-smoke.py')],cwd=ROOT,env=environment,check=True)
    if any(flag in sys.argv for flag in ['--postgres', '--teams', '--events', '--queue']):
        url = ROOT/'.local/database-url'
        if not environment.get('ORANGE_TEST_DATABASE_URL'):
            if not url.is_file():
                raise SystemExit('Real PostgreSQL required: run scripts/local-postgres.py start first')
            environment['ORANGE_TEST_DATABASE_URL'] = url.read_text().strip()
        for flag, name in [('--postgres', 'postgres'), ('--teams', 'teams'), ('--events','events'), ('--queue','queue')]:
            if flag in sys.argv:
                command=('test','-p','orange-control-plane','--features','integration-tests','--test',name,'--locked')
                if name=='postgres':
                    # The migration-under-test needs schema 66, while its sibling tests require current schema.
                    from owned_postgres import database_at
                    with database_at(environment['ORANGE_TEST_DATABASE_URL'],'orange_pg_upgrade_') as (_,upgrade_url):
                        run(*command,env=dict(environment,ORANGE_TEST_UPGRADE_DATABASE_URL=upgrade_url))
                else:
                    run(*command)
