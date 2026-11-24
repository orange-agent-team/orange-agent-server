#!/usr/bin/env python3
"""Real user OIDC/organization acceptance across explicitly selected Web and server checkouts."""
from acceptance_origins import reserve_origins
import argparse
import json
import tempfile
from owned_postgres import database
from urllib.parse import unquote,urlsplit
import os
from pathlib import Path
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import time
import urllib.request
import urllib.error
ROOT=Path(__file__).resolve().parents[1]
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--web-repo',required=True,type=Path);parser.add_argument('--node',default=shutil.which('node'))
    parser.add_argument('--prepared-environment',type=Path,help='Use the explicitly prepared local PostgreSQL and two Keycloak clients for this source checkout')
    parser.add_argument('--server-repo',type=Path,default=ROOT,help='Service sources and Rust tests to build, including a historical checkout')
    parser.add_argument('--web-driver',type=Path,help='Independent Playwright fixture checkout with the same user-session contract')
    args=parser.parse_args();server=args.server_repo.resolve(strict=True)
    reserve_origins(43122)
    web=args.web_repo.resolve()
    driver=args.web_driver.resolve(strict=True) if args.web_driver else web
    if any((web/rel).read_bytes()!=(driver/rel).read_bytes() for rel in ('docs/contracts/user.schema.json','apps/web/src/api/contracts.ts')):
        raise SystemExit("The independent fixture does not match the tested Web identity contract")
    if not (web/'apps/web/playwright.oidc.config.ts').is_file():raise SystemExit('Explicit user Web checkout required')
    if subprocess.check_output([args.node,'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Node22.23.2 required')
    for port in [43102,43103,43122]:
        with socket.socket() as probe:
            if probe.connect_ex(('127.0.0.1',port))==0:raise SystemExit(f'Port {port} occupied; stop the known development instance before isolated test')
    env=os.environ.copy();rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    env['PATH']=str(cargo.parent)+os.pathsep+str(Path(args.node).parent)+os.pathsep+env.get('PATH','')
    def run(command,cwd=server):subprocess.run([str(x) for x in command],cwd=cwd,env=env,check=True)
    private=(args.prepared_environment or server/'.local').resolve()
    if args.prepared_environment is None:
        run([sys.executable,server/'scripts/local-postgres.py','start'])
        run([sys.executable,server/'scripts/local-user-identity.py'])
    db_file=private/'database-url';user_cfg=private/'identity/user-config.json';admin_cfg=private/'identity/admin-config.json'
    database_url=db_file.read_text().strip();db=urlsplit(database_url)
    user=json.loads(user_cfg.read_bytes());admin=json.loads(admin_cfg.read_bytes())
    if (db.scheme not in ('postgres','postgresql') or db.hostname!='127.0.0.1' or db.port!=55432 or db.path!='/orange_dev' or db.query or db.fragment
            or user.get('environment')!='dev' or admin.get('environment')!='dev' or user['client_id']==admin['client_id']
            or user['user_origin']!='https://127.0.0.1:43122' or admin['admin_origin']!='https://127.0.0.1:43112'
            or any(urlsplit(config['issuer']).hostname not in ('127.0.0.1','localhost','::1') for config in (user,admin))):
        raise SystemExit('Separate loopback development identity clients and the dedicated local database are required')
    run([cargo,'build','--bin','orange-user-api','--bin','orange-admin-api','--locked'])
    run([sys.executable,web/'scripts/local-tls.py'])
    env.pop('ORANGE_ARTIFACT_ROOT',None)  # identity-only tests must not open an operator's storage root
    def schema_version(url):
        parsed=urlsplit(url)
        pg=dict(env,PGHOST=parsed.hostname,PGPORT=str(parsed.port),PGUSER=unquote(parsed.username),PGPASSWORD=unquote(parsed.password),PGDATABASE=parsed.path[1:])
        psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
        def scalar(sql):return subprocess.check_output([psql,'-X','-w','-Atq','-v','ON_ERROR_STOP=1','-c',sql],env=pg,text=True).strip()
        if scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")=='t':return 0
        return int(scalar("SELECT COALESCE(max(version),0) FROM public._sqlx_migrations WHERE success"))
    before=schema_version(database_url)
    with database(db_file,'orange_user_oidc_') as (name,owned_url),tempfile.TemporaryDirectory(prefix='orange-user-identity-') as temporary:
        owned_file=Path(temporary)/'database-url'
        with open(owned_file,'x',opener=lambda path,flags:os.open(path,flags,0o600)) as stream:
            stream.write(owned_url+'\n')
        env.update(ORANGE_TEST_DATABASE_URL=owned_url,ORANGE_TEST_USER_CONFIG_FILE=str(user_cfg),ORANGE_TEST_OIDC_CONFIG_FILE=str(admin_cfg),
            ORANGE_IDENTITY_FIXTURE=str(private/'identity/fixture.json'),ORANGE_TEST_DATABASE_URL_FILE=str(owned_file),
            ORANGE_TEST_USER_OIDC_DATABASE=name,ORANGE_USER_TLS_DIR=str(web/'.local/tls'))
        verify_owned(env,cargo,server,web,driver,args.node,owned_url,user_cfg,admin_cfg)
        installed=schema_version(owned_url)
        expected=len(list((server/'services/control-plane/migrations').glob('*.sql')))
        if installed!=expected:raise ValueError("Disposable schema did not match tested service source")
    after=schema_version(database_url)
    print(json.dumps({"source":subprocess.check_output(["git","rev-parse","HEAD"],cwd=server,text=True).strip(),
                      "web":subprocess.check_output(["git","rev-parse","HEAD"],cwd=web,text=True).strip(),
                      "owned_schema":installed,"maintenance_schema_before":before,"maintenance_schema_after":after,
                      "owned_database_removed":True}),flush=True)

def verify_owned(env,cargo,server,web,driver,node,owned_url,user_cfg,admin_cfg):
    def run(command,cwd=server):
        subprocess.run([str(x) for x in command],cwd=cwd,env=env,check=True)
    run([cargo,'test','-p','orange-control-plane','--features','integration-tests','--test','user_identity','--locked'])
    logdir=ROOT/'.local/user-e2e';logdir.mkdir(parents=True,exist_ok=True,mode=0o700);processes=[];logs=[]
    def launch(command,cwd,name,role_env=None):
        log=open(logdir/name,'w',opener=lambda p,f:os.open(p,f,0o600));logs.append(log)
        child=subprocess.Popen([str(x) for x in command],cwd=cwd,env=role_env or env,stdout=log,stderr=log);processes.append(child)
    def ready(url,status):
        context=ssl._create_unverified_context()  # Loopback test certificate only.
        for _ in range(100):
            if any(p.poll() is not None for p in processes):raise RuntimeError('Owned test process stopped; inspect private user-e2e logs')
            try:
                with urllib.request.urlopen(url,context=context,timeout=1) as response:actual=response.status
            except urllib.error.HTTPError as error:actual=error.code
            except OSError:time.sleep(.2);continue
            if actual==status:return
            time.sleep(.2)
        raise RuntimeError('User test service readiness failed')
    try:
        meta=json.loads(subprocess.check_output([cargo,'metadata','--no-deps','--locked','--format-version','1'],cwd=server,env=env))
        programs=Path(meta['target_directory'])/'debug'
        roles=dict(env,ORANGE_ENV='development',DATABASE_URL=owned_url)
        launch([programs/'orange-user-api'],server,'user-api.log',dict(roles,ORANGE_USER_CONFIG_FILE=str(user_cfg),ORANGE_USER_BIND='127.0.0.1:43103'))
        ready('http://127.0.0.1:43103/api/v1/session',401)
        launch([programs/'orange-admin-api'],server,'admin-api.log',dict(roles,ORANGE_ADMIN_CONFIG_FILE=str(admin_cfg),ORANGE_ADMIN_BIND='127.0.0.1:43102'))
        ready('http://127.0.0.1:43102/admin/v1/session',401)
        launch([node,web/'apps/web/node_modules/vite/bin/vite.js'],web/'apps/web','vite.log');ready('https://127.0.0.1:43122/',200)
        run([node,'node_modules/@playwright/test/cli.js','test','--config','playwright.oidc.config.ts'],driver/'apps/web')
        print('PASS actual separate user OIDC, PostgreSQL, HTTPS/Chrome and organization flow; no hosted Agent or payment certification')
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()
        for log in logs:log.close()
if __name__=='__main__':main()
