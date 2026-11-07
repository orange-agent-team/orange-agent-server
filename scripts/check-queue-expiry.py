#!/usr/bin/env python3
"""Actual queue-only deadline finalization, with separate resource authority."""
import os
from pathlib import Path
import subprocess
import shutil
import urllib.parse
import uuid
ROOT=Path(__file__).resolve().parents[1]
env=os.environ.copy()
for flag in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WEB_COMPUTE_TESTS','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS','ORANGE_WITHDRAW_TESTS','ORANGE_QUEUE_EXPIRY_TESTS']:
    env.pop(flag,None)
env['ORANGE_QUEUE_EXPIRY_TESTS']='1'
# The maintenance daemon scans the real queue, not a test-owner filter. Give it
# an actual isolated database so unrelated historical fixtures cannot determine
# whether this test's Job happens to land in its first bounded batch.
source=urllib.parse.urlsplit(env.get('ORANGE_TEST_DATABASE_URL') or (ROOT/'.local/database-url').read_text().strip())
if source.scheme not in ('postgres','postgresql') or source.hostname not in ('127.0.0.1','localhost','::1'):
    raise SystemExit('Queue-expiration acceptance requires an explicit local PostgreSQL URL')
psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
pg=env.copy()
pg.update(PGHOST=source.hostname,PGPORT=str(source.port or 5432),PGUSER=urllib.parse.unquote(source.username or ''),PGPASSWORD=urllib.parse.unquote(source.password or ''),PGDATABASE=urllib.parse.unquote(source.path.lstrip('/')))
pg['PGCONNECT_TIMEOUT']='5'
for key,value in urllib.parse.parse_qsl(source.query):
    if key=='sslmode':pg['PGSSLMODE']=value
name='orange_expiry_test_'+uuid.uuid4().hex
def query(sql):
    return subprocess.check_output([psql,'-X','-w','-v','ON_ERROR_STOP=1','-At','-c',sql],env=pg,text=True).strip()
query('CREATE DATABASE "'+name+'"')
print('Actual isolated maintenance test database: '+name,flush=True)
try:
    env['ORANGE_TEST_DATABASE_URL']=urllib.parse.urlunsplit(source._replace(path='/'+name))
    subprocess.run([os.sys.executable,ROOT/'scripts/check-worker-session.py'],cwd=ROOT,env=env,check=True)
finally:
    # The owned test process has exited. Never force-disconnect other sessions.
    if query("SELECT count(*) FROM pg_stat_activity WHERE datname='"+name+"'")!='0':
        raise RuntimeError('Owned acceptance database still has connections; retained '+name)
    query('DROP DATABASE "'+name+'"')
print('PASS actual deadline/bucket/ledger/reference checks and owned API maintenance; start-granted work remains separate, no physical resource-release certification')
