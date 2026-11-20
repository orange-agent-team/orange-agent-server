#!/usr/bin/env python3
"""Run the user consent and account tests against a newly created loopback PG database."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import unquote, urlsplit, urlunsplit
import uuid

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--database-url-file', required=True, type=Path)
parser.add_argument('--identity-config', required=True, type=Path)
parser.add_argument('--admin-config',type=Path)
parser.add_argument('--admin-repo',type=Path)
parser.add_argument('--web-repo',type=Path)
parser.add_argument('--node',type=Path,default=shutil.which('node'))
args = parser.parse_args()
base = urlsplit(args.database_url_file.read_text().strip())
if base.scheme not in ('postgres', 'postgresql') or base.hostname not in ('localhost', '127.0.0.1', '::1'):
    raise SystemExit('An explicit local PostgreSQL database is required')
config = json.loads(args.identity_config.read_text())
if config.get('environment') != 'dev' or urlsplit(config['issuer']).hostname not in ('127.0.0.1', 'localhost', '::1'):
    raise SystemExit('Development identity on loopback is required')

name = 'orange_consent_' + uuid.uuid4().hex
pg = dict(os.environ, PGHOST=base.hostname, PGPORT=str(base.port or 5432), PGUSER=unquote(base.username or ''),
          PGPASSWORD=unquote(base.password or ''), PGDATABASE=unquote(base.path[1:]), PGCONNECT_TIMEOUT='5')
psql = shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
def query(statement):
    return subprocess.check_output([psql, '-X', '-w', '-At', '-v', 'ON_ERROR_STOP=1', '-c', statement], env=pg, text=True).strip()

query(f'CREATE DATABASE "{name}"')
try:
    env = dict(os.environ, ORANGE_TEST_DATABASE_URL=urlunsplit(base._replace(path='/'+name)),
               ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()), CARGO_NET_OFFLINE='true')
    for key in ['ORANGE_SUPPORT_BROWSER_DRIVER','ORANGE_SUPPORT_BROWSER_NODE','ORANGE_SUPPORT_EVIDENCE']:
        env.pop(key,None)
    env.pop('ORANGE_TEST_SUPPORT_ADMIN_CONFIG', None)
    if args.admin_config:
        admin=json.loads(args.admin_config.read_text())
        if admin.get('environment')!='dev' or urlsplit(admin['issuer']).hostname not in ('127.0.0.1','localhost','::1'):
            raise SystemExit('Development Admin identity on loopback required')
        env['ORANGE_TEST_SUPPORT_ADMIN_CONFIG']=str(args.admin_config.resolve())
    env.pop('ORANGE_SUPPORT_ADMIN_DRIVER',None)
    env.pop('ORANGE_SUPPORT_ADMIN_NODE',None)
    env.pop('ORANGE_SUPPORT_ADMIN_EVIDENCE',None)
    if args.admin_repo and not args.admin_config:
        raise SystemExit('Admin browser requires the configured development Admin identity')
    for repo,is_admin in [(args.web_repo,False),(args.admin_repo,True)]:
        if not repo:
            continue
        repo=repo.resolve()
        node=Path(args.node).resolve()
        if subprocess.check_output([node,'--version'],text=True).strip()!='v22.23.2':
            raise SystemExit('Pinned Node22.23.2 required')
        snapshot=json.loads((repo/'docs/contracts/snapshot.json').read_text())
        for filename,item in snapshot['files'].items():
            if (hashlib.sha256((root/item['source']).read_bytes()).hexdigest()!=item['sha256'] or
                hashlib.sha256((repo/'docs/contracts'/filename).read_bytes()).hexdigest()!=item['sha256']):
                raise SystemExit('Consumer contract mismatch: '+filename)
        subprocess.run([node,repo/'scripts/check.mjs'],cwd=repo,check=True)
        subprocess.run([sys.executable,repo/'scripts/local-tls.py'],cwd=repo,check=True)
        evidence=root/'.local'/'support-consent'/name/('admin' if is_admin else 'user')
        evidence.mkdir(mode=0o700,parents=True)
        if is_admin:
            env.update(ORANGE_SUPPORT_ADMIN_DRIVER=str(repo/'apps/admin/scripts/real-support-grant.mjs'),
                       ORANGE_SUPPORT_ADMIN_NODE=str(node),ORANGE_SUPPORT_ADMIN_EVIDENCE=str(evidence))
        else:
            env.update(ORANGE_SUPPORT_BROWSER_DRIVER=str(repo/'apps/web/scripts/real-support-driver.mjs'),
                       ORANGE_SUPPORT_BROWSER_NODE=str(node),ORANGE_SUPPORT_EVIDENCE=str(evidence))
    subprocess.run([sys.executable, 'scripts/check-user-accounts.py'], cwd=root, env=env, check=True)
    for variable in ['ORANGE_SUPPORT_ADMIN_EVIDENCE','ORANGE_SUPPORT_EVIDENCE']:
        if variable in env:
            receipt=json.loads((Path(env[variable])/'browser-result.json').read_text())
            if receipt['mutations'] != (6 if variable=='ORANGE_SUPPORT_ADMIN_EVIDENCE' else 2):
                raise SystemExit('Browser receipt does not attest the requested workflow')
finally:
    if query("SELECT count(*) FROM pg_stat_activity WHERE datname='"+name+"'") == '0':
        query(f'DROP DATABASE "{name}"')
        print('Removed owned test database '+name, flush=True)
    else:
        raise SystemExit('Test database retained with open connections: '+name)
