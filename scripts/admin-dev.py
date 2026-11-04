#!/usr/bin/env python3
"""Run the development Admin API with explicitly provisioned local test identities."""
import argparse
import json
import os
from pathlib import Path
import subprocess
from urllib.parse import urlparse, unquote

ROOT = Path(__file__).resolve().parents[1]
def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['provision', 'serve'])
    action = parser.parse_args().action
    url = (ROOT/'.local/database-url').read_text().strip()
    parsed = urlparse(url)
    if parsed.hostname != '127.0.0.1' or parsed.port != 55432 or parsed.path != '/orange_dev':
        raise SystemExit('Local fixture requires dedicated development database')
    env = os.environ.copy()
    if action == 'serve':
        env.update(ORANGE_ENV='development', DATABASE_URL=url,
                   ORANGE_ADMIN_CONFIG_FILE=str(ROOT/'.local/identity/admin-config.json'))
        os.execve(str(ROOT/'target/debug/orange-admin-api'), ['orange-admin-api'], env)
    value = json.loads((ROOT/'.local/identity/fixture.json').read_text())
    issuer = 'http://127.0.0.1:44180/realms/orange-dev'
    def literal(v):
        return "'"+v.replace("'", "''")+"'"
    sql = ['BEGIN;']
    for user in value['users']:
        principal, subject, role = map(literal, [user['principal_id'], user['subject'], user['role']])
        sql += [f"INSERT INTO admin_principals(principal_id,issuer,subject) VALUES({principal},{literal(issuer)},{subject}) ON CONFLICT(principal_id) DO NOTHING;",
                f"INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES({principal},{role},'dev','platform','platform',EXTRACT(EPOCH FROM clock_timestamp())::bigint+86400) ON CONFLICT(principal_id,role,environment,scope_kind,scope_id) DO UPDATE SET revoked=FALSE,expires_at=EXCLUDED.expires_at;"]
    sql.append('COMMIT;')
    env.update(PGHOST=parsed.hostname, PGPORT=str(parsed.port), PGUSER=unquote(parsed.username),
               PGPASSWORD=unquote(parsed.password), PGDATABASE=parsed.path[1:])
    subprocess.run(['/opt/homebrew/opt/postgresql@17/bin/psql', '-X', '-v', 'ON_ERROR_STOP=1', '-q'],
                   input='\n'.join(sql), text=True, env=env, check=True)
    print('Two local test principals provisioned with one-day dev-only grants; no production identities')
if __name__ == '__main__':
    main()
