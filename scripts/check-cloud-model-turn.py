#!/usr/bin/env python3
"""Run the built model-funded hosted Run with one actual CPU provider and isolated Host."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit
from owned_postgres import database
from platform_test_pki import prepare
root=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser()
p.add_argument('--database-url-file',required=True,type=Path)
p.add_argument('--identity-config',required=True,type=Path)
p.add_argument('--web-repo',required=True,type=Path)
p.add_argument('--model-runtime-root',type=Path,default=root/'.local/model-runtime')
p.add_argument('--node',type=Path,default=shutil.which('node'))
args=p.parse_args()
identity=json.loads(args.identity_config.read_text())
if identity.get('environment')!='dev' or urlsplit(identity['issuer']).hostname not in ('localhost','127.0.0.1','::1'):
    raise SystemExit('Isolated development issuer on loopback required')
web=args.web_repo.resolve()
snapshot=json.loads((web/'docs/contracts/snapshot.json').read_text())
for name,entry in snapshot['files'].items():
    if any(hashlib.sha256(path.read_bytes()).hexdigest()!=entry['sha256'] for path in [root/entry['source'],web/'docs/contracts'/name]):
        raise SystemExit('Producer/consumer contract differs: '+name)
os.umask(0o077)
with database(args.database_url_file,'orange_cloud_model_') as (name,url),tempfile.TemporaryDirectory() as directory:
    tls=Path(directory);prepare(tls)
    private=tls/'database-url';private.write_text(url+'\n');private.chmod(0o600)
    evidence=root/'.local'/'cloud-model-turn'/name;evidence.mkdir(parents=True,mode=0o700)
    env=dict(os.environ,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),
             ORANGE_TEST_MODEL_RUNTIME_ROOT=str(args.model_runtime_root.resolve()),ORANGE_CLOUD_MODEL_BROWSER=str(web/'apps/web/scripts/real-cloud-model.mjs'),
             ORANGE_CLOUD_MODEL_QA_DIR=str(tls),ORANGE_CLOUD_MODEL_EVIDENCE=str(evidence),ORANGE_CLOUD_QA_PYTHON=sys.executable)
    subprocess.run([sys.executable,root/'scripts/check-web-compute.py','--web-repo',web,'--node',args.node],env=env,cwd=root,check=True)
    result=json.loads((evidence/'cloud-model-result.json').read_text())
    if result.get('settlement_count')!=1 or not result.get('result_denied_after_delete') or result.get('cloud_workspace')!='deleted':
        raise SystemExit('Incomplete scoped model settlement and result evidence')
    print('PASS hosted model Run, actual CPU provider and development ledger. Evidence: '+str(evidence.relative_to(root)))
