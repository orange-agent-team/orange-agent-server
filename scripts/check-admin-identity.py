#!/usr/bin/env python3
"""Real cross-repository identity acceptance. Explicit Admin checkout; no build-time sibling dependency."""
import argparse
import os
from pathlib import Path
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--admin-repo', required=True, type=Path)
    parser.add_argument('--node', default=shutil.which('node'))
    args = parser.parse_args()
    admin = args.admin_repo.resolve()
    if not (admin/'apps/admin/playwright.oidc.config.ts').is_file():
        raise SystemExit('Explicit Admin checkout with identity acceptance tests required')
    if subprocess.check_output([args.node, '--version'], text=True).strip() != 'v22.23.2':
        raise SystemExit('Node 22.23.2 required')
    for port in [43102, 43112]:
        with socket.socket() as check:
            if check.connect_ex(('127.0.0.1', port)) == 0:
                raise SystemExit(f'Port {port} occupied; stop the known development instance before isolated test')
    env = os.environ.copy()
    rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
    env['PATH'] = str(cargo.parent)+os.pathsep+str(Path(args.node).parent)+os.pathsep+env.get('PATH', '')
    def run(cmd, cwd=ROOT):
        subprocess.run([str(x) for x in cmd], cwd=cwd, env=env, check=True)
    run([sys.executable, ROOT/'scripts/local-postgres.py', 'start'])
    run([sys.executable, ROOT/'scripts/local-identity.py', 'start'])
    run([cargo, 'build', '--bin', 'orange-admin-api', '--locked'])
    run([sys.executable, admin/'scripts/local-tls.py'])
    env.update(ORANGE_ADMIN_TLS_DIR=str(admin/'.local/tls'),
               ORANGE_TEST_DATABASE_URL=(ROOT/'.local/database-url').read_text().strip(),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(ROOT/'.local/identity/admin-config.json'),
               ORANGE_IDENTITY_FIXTURE=str(ROOT/'.local/identity/fixture.json'),
               ORANGE_TEST_DATABASE_URL_FILE=str(ROOT/'.local/database-url'))
    run([cargo, 'test', '-p', 'orange-control-plane', '--features', 'integration-tests', '--test', 'admin_identity', '--locked'])
    processes, logs = [], []
    logdir = ROOT/'.local/admin-e2e'
    logdir.mkdir(parents=True, exist_ok=True, mode=0o700)
    def launch(command, cwd, name):
        log = open(logdir/name, 'w', opener=lambda p, flags: os.open(p, flags, 0o600))
        logs.append(log)
        child = subprocess.Popen([str(x) for x in command], cwd=cwd, env=env, stdout=log, stderr=log)
        processes.append(child)
        return child
    def ready(url, expected):
        context = ssl._create_unverified_context()  # Dedicated loopback test certificate only.
        for _ in range(100):
            if any(p.poll() is not None for p in processes):
                raise RuntimeError('Owned test process stopped; inspect private .local/admin-e2e logs')
            try:
                with urllib.request.urlopen(url, timeout=1, context=context) as response:
                    status = response.status
            except urllib.error.HTTPError as error:
                status = error.code
            except OSError:
                time.sleep(.2)
                continue
            if status == expected:
                return
            time.sleep(.2)
        raise RuntimeError('Local service readiness failed')
    try:
        launch([sys.executable, ROOT/'scripts/admin-dev.py', 'serve'], ROOT, 'api.log')
        ready('http://127.0.0.1:43102/admin/v1/session', 401)
        run([sys.executable, ROOT/'scripts/admin-dev.py', 'provision'])
        launch([args.node, admin/'apps/admin/node_modules/vite/bin/vite.js'], admin/'apps/admin', 'vite.log')
        ready('https://127.0.0.1:43112/', 200)
        run([args.node, 'node_modules/@playwright/test/cli.js', 'test', '--config', 'playwright.oidc.config.ts'], admin/'apps/admin')
        print('PASS: actual Keycloak password/TOTP + Rust OIDC + PostgreSQL + HTTPS/Chrome; development only')
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for log in logs:
            log.close()
        # Dedicated shared PostgreSQL/Keycloak persist for development; owned API/Vite always stop.
if __name__ == '__main__':
    main()
