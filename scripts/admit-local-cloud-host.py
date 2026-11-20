#!/usr/bin/env python3
"""Admit one explicitly identified development Host after live local isolation qualification."""
import argparse
import datetime
import email.utils
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
from urllib.parse import unquote,urlsplit
root=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument('--database-url-file',type=Path,required=True)
p.add_argument('--host-cert',type=Path,required=True)
p.add_argument('--platform-host-ca',type=Path,required=True)
p.add_argument('--host-id',required=True)
p.add_argument('--archive',type=Path)
args=p.parse_args()
if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.:-]{0,127}',args.host_id):
    raise SystemExit('Explicit stable Host instance identifier required')
base=urlsplit(args.database_url_file.read_text().strip())
if base.scheme not in ('postgres','postgresql') or base.hostname not in ('127.0.0.1','localhost','::1'):
    raise SystemExit('Explicit local development PostgreSQL required')
openssl=Path('/opt/homebrew/opt/openssl@3/bin/openssl')
if not openssl.is_file():openssl=shutil.which('openssl')
subprocess.run([openssl,'verify','-purpose','sslclient','-CAfile',args.platform_host_ca,args.host_cert],check=True)
def cert(*arguments):
    return subprocess.check_output([openssl,'x509','-in',args.host_cert,*arguments])
if b'CA:FALSE' not in cert('-noout','-ext','basicConstraints') or b'TLS Web Client Authentication' not in cert('-noout','-ext','extendedKeyUsage'):
    raise SystemExit('A non-CA platform client leaf with clientAuth is required')
leaf=hashlib.sha256(cert('-outform','DER')).hexdigest()
run=[sys.executable,str(root/'scripts/check-cloud-isolation.py')]
if args.archive:run+=['--archive',str(args.archive)]
subprocess.run(run,cwd=root,check=True)
proof_path=root/'.local/cloud-isolation/result.json'
proof=json.loads(proof_path.read_text())
policy=json.loads((root/'config/cloud-isolation.lock.json').read_text())
if not proof['qualified'] or not proof['sandbox_and_cgroup_removed'] or proof['archive_sha256']!=policy['sha256']:
    raise SystemExit('Incomplete local qualification evidence')
digest=hashlib.sha256(json.dumps(proof,sort_keys=True,separators=(',',':')).encode()).hexdigest()
deadline=email.utils.parsedate_to_datetime(cert('-enddate','-noout').decode().strip().split('=',1)[1]).astimezone(datetime.timezone.utc)
if deadline<=datetime.datetime.now(datetime.timezone.utc):
    raise SystemExit('Platform client credential expired')
env=dict(os.environ,PGHOST=base.hostname,PGPORT=str(base.port or 5432),PGUSER=unquote(base.username or ''),
         PGPASSWORD=unquote(base.password or ''),PGDATABASE=unquote(base.path[1:]),PGCONNECT_TIMEOUT='5')
psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
# psql's quoted variables supply literal values. An older credential or a
# revoked identity is never silently extended or replaced by this tool.
sql=r"""
BEGIN;
INSERT INTO cloud_pool_hosts(host_instance_id,certificate_sha256,profile_id,attestation_digest,region_id,max_workspaces,credential_expires_at)
SELECT :'host',:'leaf',:'profile',:'proof','dev-local',1,LEAST(:'until'::timestamptz,clock_timestamp()+interval '24 hours')
WHERE NOT EXISTS(SELECT 1 FROM worker_credentials WHERE certificate_sha256=:'leaf')
ON CONFLICT DO NOTHING;
SELECT EXISTS(SELECT 1 FROM cloud_pool_hosts WHERE host_instance_id=:'host' AND certificate_sha256=:'leaf'
 AND profile_id=:'profile' AND attestation_digest=:'proof' AND region_id='dev-local'
 AND enabled AND credential_expires_at>clock_timestamp()
 AND NOT EXISTS(SELECT 1 FROM worker_credentials WHERE certificate_sha256=:'leaf')) AS accepted \gset
\if :accepted
COMMIT;
\else
ROLLBACK;
\endif
SELECT :'accepted';
"""
command=[psql,'-X','-w','-qAt','-v','ON_ERROR_STOP=1']
for key,value in [('host',args.host_id),('leaf',leaf),('profile',policy['profile']),('proof',digest),('until',deadline.isoformat())]:
    command+=['-v',key+'='+value]
result=subprocess.check_output(command,input=sql,env=env,text=True).strip()
if result!='t':raise SystemExit('Conflicting or retired Host identity; admission refused')
print(json.dumps({'host_instance_id':args.host_id,'resource_profile_id':policy['profile'],'attestation_digest':digest,
                  'scope':'development registration only; allocation requires an authenticated live heartbeat'}))
