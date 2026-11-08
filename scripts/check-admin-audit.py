#!/usr/bin/env python3
"""Scoped audit migration/HTTP tests in an owned disposable actual local PG DB."""
import argparse,json,os,shutil,socket,subprocess,sys,urllib.parse,uuid,hashlib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--database-url-file',type=Path,required=True);parser.add_argument('--identity-config',type=Path,required=True);parser.add_argument('--admin-repo',type=Path);parser.add_argument('--node',type=Path,default=shutil.which('node'));args=parser.parse_args()
source=urllib.parse.urlsplit(args.database_url_file.read_text().strip())
if source.scheme not in ('postgres','postgresql') or source.hostname not in ('127.0.0.1','localhost','::1'):raise SystemExit('Explicit local PostgreSQL required')
if not args.identity_config.is_file():raise SystemExit('Explicit private development identity configuration required')
schema=json.loads((ROOT/'contracts/json-schema/admin.schema.json').read_text())
api=json.loads((ROOT/'contracts/openapi/admin.v1.json').read_text())
def normalize(value):
    if isinstance(value,list):return [normalize(v) for v in value]
    if isinstance(value,dict):return {k:(v.replace('#/components/schemas/','#/$defs/',1) if k=='$ref' and isinstance(v,str) and v.startswith('#/components/schemas/') else normalize(v)) for k,v in value.items()}
    return value
if normalize(api['components']['schemas'])!=schema['$defs']:raise SystemExit('Admin OpenAPI and JSON Schema differ')
def references(value):
    if isinstance(value,list):
        for child in value:references(child)
    elif isinstance(value,dict):
        if '$ref' in value:
            ref=value['$ref']
            if not ref.startswith('#/'):raise SystemExit('Unexpected external contract reference')
            target=api
            for part in ref[2:].split('/'):target=target[part.replace('~1','/').replace('~0','~')]
        for child in value.values():references(child)
references(api)
print('PASS consistent Admin schemas and resolvable local OpenAPI references',flush=True)
if args.admin_repo:
    args.admin_repo=args.admin_repo.resolve()
    if not args.node or subprocess.check_output([str(args.node),'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Explicit pinned Node22.23.2 required')
    driver=args.admin_repo/'apps/admin/scripts/real-audit-driver.mjs'
    if not driver.is_file():raise SystemExit('Explicit real audit browser driver required')
    snapshot=json.loads((args.admin_repo/'docs/contracts/snapshot.json').read_text())
    for entry in snapshot['files'].values():
        if hashlib.sha256((ROOT/entry['source']).read_bytes()).hexdigest()!=entry['sha256']:raise SystemExit('Browser consumer differs from tested producer contract')
    with socket.socket() as probe:
        if probe.connect_ex(('127.0.0.1',43141))==0:raise SystemExit('Owned audit browser port43141 occupied;do not replace an unknown process')
    subprocess.run([str(args.node),str(args.admin_repo/'scripts/check.mjs')],cwd=args.admin_repo,check=True)
    subprocess.run([sys.executable,str(args.admin_repo/'scripts/local-tls.py')],cwd=args.admin_repo,check=True)
name='orange_audit_test_'+uuid.uuid4().hex
pg=os.environ.copy();pg.update(PGHOST=source.hostname,PGPORT=str(source.port or 5432),PGUSER=urllib.parse.unquote(source.username or ''),PGPASSWORD=urllib.parse.unquote(source.password or ''),PGDATABASE=urllib.parse.unquote(source.path.lstrip('/')),PGCONNECT_TIMEOUT='5')
for key,value in urllib.parse.parse_qsl(source.query):
    if key=='sslmode':pg['PGSSLMODE']=value
psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
def query(sql):return subprocess.check_output([psql,'-X','-w','-v','ON_ERROR_STOP=1','-At','-c',sql],env=pg,text=True).strip()
query('CREATE DATABASE "'+name+'"')
folder=ROOT/'.local/audit-acceptance'/name;folder.mkdir(parents=True,mode=0o700)
url=urllib.parse.urlunsplit(source._replace(path='/'+name));private=folder/'database-url';fd=os.open(private,os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)
with os.fdopen(fd,'w') as stream:stream.write(url+'\n')
result={'database':name,'passed':False,'dropped':False,'scope':'actual PostgreSQL migrations and HTTP scopes/cursors;explicit session fixtures,not MFA issuance or browser certification'}
try:
    cargo=Path(subprocess.check_output([shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup','which','--toolchain','stable','cargo'],text=True).strip())
    env=os.environ.copy();env.update(PATH=str(cargo.parent)+os.pathsep+env.get('PATH',''),ORANGE_TEST_DATABASE_URL=url,ORANGE_AUDIT_TEST_DATABASE=name,ORANGE_TEST_OIDC_CONFIG_FILE=str(args.identity_config.resolve()),CARGO_TARGET_DIR=str(ROOT/'target'),CARGO_BUILD_JOBS='2',CARGO_NET_OFFLINE='true')
    for key in ['ORANGE_AUDIT_BROWSER_DRIVER','ORANGE_AUDIT_BROWSER_NODE','ORANGE_AUDIT_BROWSER_EVIDENCE']:env.pop(key,None)
    if args.admin_repo:
        env.update(ORANGE_AUDIT_BROWSER_DRIVER=str(driver),ORANGE_AUDIT_BROWSER_NODE=str(args.node.resolve()),ORANGE_AUDIT_BROWSER_EVIDENCE=str(folder))
        result['browser_requested']=True
    for command in [
        ['test','-p','orange-control-plane','--features','integration-tests','--test','admin_audit','--locked','--offline'],
        ['clippy','-p','orange-control-plane','--features','integration-tests','--test','admin_audit','--locked','--offline','--','-D','warnings'],
    ]:subprocess.run([str(cargo),*command],cwd=ROOT,env=env,check=True)
    result['passed']=True
finally:
    if result['passed'] and query("SELECT count(*) FROM pg_stat_activity WHERE datname='"+name+"'")=='0':
        query('DROP DATABASE "'+name+'"');result['dropped']=True;private.unlink()
    (folder/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result),flush=True)
if not result['dropped']:raise SystemExit('Owned acceptance database retained;never force-disconnect sessions')
