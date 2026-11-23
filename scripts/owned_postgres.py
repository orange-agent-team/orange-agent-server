"""Disposable test database on an explicitly provided loopback PostgreSQL server."""
from contextlib import contextmanager
import os
import re
import shutil
import subprocess
from urllib.parse import unquote,urlsplit,urlunsplit
import uuid

def database(url_file,prefix):
    return database_at(url_file.read_text().strip(),prefix)


@contextmanager
def database_at(url,prefix):
    if not re.fullmatch(r'orange_[a-z_]{1,20}_',prefix):
        raise ValueError('Test database prefix required')
    base=urlsplit(url)
    if base.scheme not in ('postgres','postgresql') or base.hostname not in ('127.0.0.1','localhost','::1'):
        raise ValueError('Explicit loopback PostgreSQL database required')
    name=prefix+uuid.uuid4().hex
    env=dict(os.environ,PGHOST=base.hostname,PGPORT=str(base.port or 5432),PGUSER=unquote(base.username or ''),
             PGPASSWORD=unquote(base.password or ''),PGDATABASE=unquote(base.path[1:]),PGCONNECT_TIMEOUT='5')
    psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
    def query(sql):
        return subprocess.check_output([psql,'-X','-w','-At','-v','ON_ERROR_STOP=1','-c',sql],env=env,text=True).strip()
    query(f'CREATE DATABASE "{name}"')
    try:
        yield name,urlunsplit(base._replace(path='/'+name))
    finally:
        if query("SELECT count(*) FROM pg_stat_activity WHERE datname='"+name+"'")!='0':
            raise RuntimeError('Owned database retained with open connections: '+name)
        query(f'DROP DATABASE "{name}"')
        print('Removed owned test database '+name,flush=True)
