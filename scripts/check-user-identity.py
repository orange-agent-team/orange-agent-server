#!/usr/bin/env python3
"""Real user OIDC/organization acceptance across explicitly selected Web and server checkouts."""
from acceptance_origins import reserve_origins
import argparse
import json
from urllib.parse import urlsplit
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
    args=parser.parse_args()
    reserve_origins(43122)
    web=args.web_repo.resolve()
    if not (web/'apps/web/playwright.oidc.config.ts').is_file():raise SystemExit('Explicit user Web checkout required')
    if subprocess.check_output([args.node,'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Node22.23.2 required')
    for port in [43102,43103,43122]:
        with socket.socket() as probe:
            if probe.connect_ex(('127.0.0.1',port))==0:raise SystemExit(f'Port {port} occupied; stop the known development instance before isolated test')
    env=os.environ.copy();rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    env['PATH']=str(cargo.parent)+os.pathsep+str(Path(args.node).parent)+os.pathsep+env.get('PATH','')
    def run(command,cwd=ROOT):subprocess.run([str(x) for x in command],cwd=cwd,env=env,check=True)
    private=(args.prepared_environment or ROOT/'.local').resolve()
    if args.prepared_environment is None:
        run([sys.executable,ROOT/'scripts/local-postgres.py','start'])
        run([sys.executable,ROOT/'scripts/local-user-identity.py'])
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
    env.update(ORANGE_TEST_DATABASE_URL=database_url,ORANGE_TEST_USER_CONFIG_FILE=str(user_cfg),ORANGE_TEST_OIDC_CONFIG_FILE=str(admin_cfg),
        ORANGE_IDENTITY_FIXTURE=str(private/'identity/fixture.json'),ORANGE_TEST_DATABASE_URL_FILE=str(db_file),ORANGE_USER_TLS_DIR=str(web/'.local/tls'))
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
        if args.prepared_environment is None:
            launch([sys.executable,ROOT/'scripts/user-dev.py'],ROOT,'user-api.log')
            ready('http://127.0.0.1:43103/api/v1/session',401)
            launch([sys.executable,ROOT/'scripts/admin-dev.py','serve'],ROOT,'admin-api.log')
        else:
            meta=json.loads(subprocess.check_output([cargo,'metadata','--no-deps','--locked','--format-version','1'],cwd=ROOT,env=env))
            programs=Path(meta['target_directory'])/'debug'
            roles=dict(env,ORANGE_ENV='development',DATABASE_URL=database_url)
            launch([programs/'orange-user-api'],ROOT,'user-api.log',dict(roles,ORANGE_USER_CONFIG_FILE=str(user_cfg),ORANGE_USER_BIND='127.0.0.1:43103'))
            ready('http://127.0.0.1:43103/api/v1/session',401)
            launch([programs/'orange-admin-api'],ROOT,'admin-api.log',dict(roles,ORANGE_ADMIN_CONFIG_FILE=str(admin_cfg),ORANGE_ADMIN_BIND='127.0.0.1:43102'))
        ready('http://127.0.0.1:43102/admin/v1/session',401)
        launch([args.node,web/'apps/web/node_modules/vite/bin/vite.js'],web/'apps/web','vite.log');ready('https://127.0.0.1:43122/',200)
        run([args.node,'node_modules/@playwright/test/cli.js','test','--config','playwright.oidc.config.ts'],web/'apps/web')
        print('PASS actual separate user OIDC, PostgreSQL, HTTPS/Chrome and organization flow; no hosted Agent or payment certification')
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()
        for log in logs:log.close()
if __name__=='__main__':main()
