#!/usr/bin/env python3
"""Exercise user Run reservation and a separately authenticated platform Host."""
from acceptance_origins import reserve_origins
import argparse
import hashlib
import sys
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from urllib.parse import urlsplit
from owned_postgres import database
from platform_test_pki import prepare

root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--database-url-file',type=Path,required=True)
parser.add_argument('--identity-config',type=Path,required=True)
parser.add_argument('--web-repo',type=Path)
parser.add_argument('--node',type=Path,default=shutil.which('node'))
parser.add_argument('--qualify-host',action='store_true',help='Run the pinned OCI qualification during private Host admission')
parser.add_argument('--managed-host',action='store_true',help='Exercise the real development Host binary, per-allocation gVisor sandbox and process restart')
args=parser.parse_args()
if args.web_repo:reserve_origins(43122)
if args.managed_host and not args.qualify_host:raise SystemExit('A managed Host test requires real --qualify-host enrollment')
config=json.loads(args.identity_config.read_text())
if config.get('environment')!='dev' or urlsplit(config['issuer']).hostname not in ('127.0.0.1','localhost','::1'):
    raise SystemExit('Loopback development identity is required')
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
with database(args.database_url_file,'orange_cloud_run_') as (database_name,url), tempfile.TemporaryDirectory() as directory:
    tls=Path(directory);os.umask(0o077)
    prepare(tls)
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ['PATH'],CARGO_NET_OFFLINE='true',
             ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),ORANGE_TEST_HOST_TLS=str(tls))
    env.pop('ORANGE_CLOUD_QUALIFIED_DATABASE_FILE',None)
    if args.qualify_host:
        private=tls/'local-database'
        private.write_text(url+'\n')
        private.chmod(0o600)
        env['ORANGE_CLOUD_QUALIFIED_DATABASE_FILE']=str(private)
        env['ORANGE_CLOUD_QA_PYTHON']=sys.executable
    env.pop('ORANGE_MANAGED_HOST_EVIDENCE',None)
    env.pop('ORANGE_CLOUD_MANAGED_BROWSER',None)
    if args.managed_host:
        evidence_dir=root/'.local'/'cloud-admission'/database_name
        evidence_dir.mkdir(parents=True,exist_ok=True,mode=0o700)
        env['ORANGE_MANAGED_HOST_EVIDENCE']=str(evidence_dir)
        env['ORANGE_CLOUD_QA_PYTHON']=sys.executable
    env.pop('ORANGE_CLOUD_WEB_DRIVER',None)
    if args.web_repo:
        repo=args.web_repo.resolve()
        node=Path(args.node).resolve()
        if subprocess.check_output([node,'--version'],text=True).strip()!='v22.23.2':
            raise SystemExit('Pinned Node 22.23.2 required')
        snapshot=json.loads((repo/'docs/contracts/snapshot.json').read_text())
        for filename,entry in snapshot['files'].items():
            if any(hashlib.sha256(p.read_bytes()).hexdigest()!=entry['sha256'] for p in [root/entry['source'],repo/'docs/contracts'/filename]):
                raise SystemExit('Contract mismatch: '+filename)
        subprocess.run([node,repo/'scripts/check.mjs'],cwd=repo,check=True)
        subprocess.run([sys.executable,repo/'scripts/local-tls.py'],cwd=repo,check=True)
        evidence=root/'.local'/'cloud-admission'/database_name
        evidence.mkdir(parents=True,exist_ok=True,mode=0o700)
        env.update(ORANGE_CLOUD_WEB_DRIVER=str(repo/'apps/web/scripts/real-cloud-run.mjs'),
                   ORANGE_CLOUD_WEB_NODE=str(node),ORANGE_CLOUD_WEB_EVIDENCE=str(evidence))
        if args.managed_host:
            env['ORANGE_CLOUD_MANAGED_BROWSER']=str(repo/'apps/web/scripts/real-managed-workspace.mjs')
    run=subprocess.run([cargo,'test','--locked','--offline','-p','orange-control-plane','--features','cloud-host-tests','--test','user_identity',
                        'cloud_host_tests','--','--test-threads=1'],cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    print(run.stdout,flush=True);run.check_returncode()
    if 'test result: ok. 1 passed;' not in run.stdout:raise SystemExit('Named cloud host test failed to run')

    if args.web_repo and json.loads((evidence/'browser-result.json').read_text()).get('creates')!=2:
        raise SystemExit('Original request recovery was not verified')
    if args.web_repo and args.qualify_host and not (evidence/'host-registration.json').is_file():
        raise SystemExit('Missing live host admission evidence')
    if args.managed_host:
        managed=json.loads((evidence_dir/'managed-host.json').read_text())
        if {r.get("cause") for r in managed}!={"user_cancel","controller_restart","input_deleted"} or not all(r.get('files_absent') and r.get('cgroup_absent') and r.get('sandbox_absent') for r in managed):
            raise SystemExit('Incomplete per-allocation real Host cleanup evidence')
    if args.managed_host and args.web_repo and not json.loads((evidence_dir/'managed-browser.json').read_text())['browser']['observed_ready']:
        raise SystemExit('Real browser workspace readiness was not observed')
