"""Provision the local Keycloak test subjects in a caller-validated development database."""
import json
import os
import shutil
import subprocess
from urllib.parse import unquote

def provision(parsed,fixture):
    env=os.environ.copy()
    value = json.loads(fixture.read_text())
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
    subprocess.run([shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql', '-X', '-v', 'ON_ERROR_STOP=1', '-q'],
                   input='\n'.join(sql), text=True, env=env, check=True)
    print('Two local test principals provisioned with one-day dev-only grants; no production identities')
