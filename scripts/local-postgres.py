#!/usr/bin/env python3
"""Manage only this repository's private PostgreSQL development cluster."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / '.local/postgres'
PORT = 55432


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['start', 'stop', 'status'])
    args = parser.parse_args()
    bins = os.environ.get('ORANGE_POSTGRES_BIN')
    if not bins:
        executable = shutil.which('pg_ctl')
        bins = str(Path(executable).parent) if executable else '/opt/homebrew/opt/postgresql@17/bin'
    directory = Path(bins)
    if not (directory / 'pg_ctl').is_file():
        raise SystemExit('PostgreSQL is not installed; set ORANGE_POSTGRES_BIN to its bin directory')
    LOCAL.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(LOCAL, 0o700)
    data = LOCAL / 'data'
    password_file = LOCAL / 'password'
    if args.action == 'start' and not password_file.exists():
        password_file.write_text(secrets.token_urlsafe(36))
        os.chmod(password_file, 0o600)
    environment = os.environ.copy()
    if password_file.exists():
        environment['PGPASSWORD'] = password_file.read_text().strip()
    def run(tool, *parts, check=True, capture=False):
        return subprocess.run([str((directory / tool).resolve()), *map(str, parts)], env=environment,
                              check=check, capture_output=capture, text=True)
    if args.action == 'stop':
        if (data / 'PG_VERSION').is_file():
            run('pg_ctl', '-D', data, 'stop', '-m', 'fast')
        return
    if args.action == 'status':
        result = run('pg_ctl', '-D', data, 'status', check=False)
        raise SystemExit(result.returncode)
    if not (data / 'PG_VERSION').exists():
        if data.exists() and any(data.iterdir()):
            raise SystemExit('Refusing to initialize a nonempty data directory')
        run('initdb', '-D', data, '-U', 'orange_local', '-A', 'scram-sha-256', '--pwfile', password_file,
            '--encoding=UTF8', '--locale=C')
    if run('pg_ctl', '-D', data, 'status', check=False, capture=True).returncode != 0:
        socket = LOCAL / 'socket'
        socket.mkdir(exist_ok=True, mode=0o700)
        run('pg_ctl', '-D', data, '-l', LOCAL / 'server.log', '-o',
            '-h 127.0.0.1 -p ' + str(PORT) + ' -k ' + str(socket), '-w', 'start')
    common = ['-h', '127.0.0.1', '-p', str(PORT), '-U', 'orange_local']
    result = run('psql', *common, '-d', 'postgres', '-tAc',
                 "SELECT 1 FROM pg_database WHERE datname='orange_dev'", capture=True)
    if result.stdout.strip() != '1':
        run('createdb', *common, 'orange_dev')
    url = 'postgresql://orange_local:' + quote(password_file.read_text().strip(), safe='') + '@127.0.0.1:' + str(PORT) + '/orange_dev'
    url_file = ROOT / '.local/database-url'
    url_file.write_text(url + '\n')
    os.chmod(url_file, 0o600)
    (LOCAL / 'identity.json').write_text(json.dumps({'data_directory': str(data), 'port': PORT,
        'purpose': 'orange-development-only', 'production': False}, indent=2) + '\n')
    print('Private development PostgreSQL ready; URL in .local/database-url (not printed)')


if __name__ == '__main__':
    main()
